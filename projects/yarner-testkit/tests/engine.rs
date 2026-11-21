//! The engine against the mock thinfer.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use yarner_engine::model::{Next, STORY_PROMPT, STYLE_PROMPT, Scope};
use yarner_engine::view::NextView;
use yarner_engine::{Advance, Config, Engine};
use yarner_format::{Install, KdfParams};
use yarner_testkit::mock::{MockThinfer, Timing};
use yarner_testkit::playtest::{self, Picker, Player};

const STORY: &str =
    "A keeper of a remote lighthouse discovers the lamp signals something under the sea.";
const DESCR: &str =
    "Mira, the keeper: weathered hands, a green oilskin coat, distrusts the mainland.";

fn install(dir: &Path) -> Install {
    Install::open_with(dir, KdfParams::insecure_fast()).unwrap()
}

fn open(inst: &Install, pw: &str, mock: Arc<MockThinfer>) -> Engine {
    let story = inst.unlock(pw).unwrap().pop().unwrap();
    // Tools on: the engine still supports them (off by default for real backends).
    let cfg = Config {
        tool_rounds: 3,
        ..Config::default()
    };
    Engine::open(story, mock, cfg).unwrap()
}

fn setup(e: &Engine) {
    e.set_prompt(STORY_PROMPT, STORY, None, Scope::FromHere)
        .unwrap();
    e.set_prompt(STYLE_PROMPT, "moody watercolor", None, Scope::FromHere)
        .unwrap();
    e.put_object(None, "Mira", DESCR, vec![], false, None, None)
        .unwrap();
    e.put_object(
        None,
        "The Lighthouse",
        "Whitewashed tower on a basalt spit.",
        vec![],
        false,
        None,
        None,
    )
    .unwrap();
}

async fn settle(e: &Engine) {
    // A wall-clock deadline, not a count of short sleeps: under load (other suites, other
    // agents) a big fill took longer than 2000 x 5 ms and the test failed while working.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if e.idle() {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if e.idle() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("engine never went idle");
}

/// Advance until a beat with choices is current.
async fn to_choice(e: &Engine, play: &str) {
    for _ in 0..200 {
        let v = e.play_view(play).unwrap();
        if let Some(b) = &v.beat
            && matches!(b.next, NextView::Choices { .. })
        {
            return;
        }
        while e.advance(play, None).unwrap() == Advance::Waiting {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    panic!("no choice reached");
}

fn fast_player(beats: usize) -> Player {
    Player {
        chars_per_sec: 1e6,
        per_line: Duration::ZERO,
        picker: Picker::Seeded(7),
        beats,
        max_wait: Duration::from_secs(20),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plays_through_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{\"title\":\"Lamp\"}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("first").unwrap();
    let r = playtest::run(&e, &play, &fast_player(25)).await;
    assert!(!r.gave_up, "{r:?}");
    assert_eq!(r.beats, 25);
    assert!(r.images_new > 0);
    assert!(
        mock.stats().tool_calls > 0,
        "storyteller should have used a tool"
    );
    settle(&e).await;
    let depth = e.play_view(&play).unwrap().depth;
    let beats = e.with_graph(|g| g.beats.len());
    drop(e);

    let e = open(&inst, "pw", mock);
    assert_eq!(e.with_graph(|g| g.beats.len()), beats);
    assert_eq!(e.play_view(&play).unwrap().depth, depth);
    // Every requested picture finished (on reopen too).
    settle(&e).await;
    let r2 = playtest::run(&e, &play, &fast_player(5)).await;
    assert_eq!(r2.beats, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speculates_every_choice_and_keeps_untaken_as_pending() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    let play = e.new_play("p").unwrap();
    to_choice(&e, &play).await;
    settle(&e).await;
    let v = e.play_view(&play).unwrap();
    let b = v.beat.unwrap();
    let NextView::Choices { choices } = &b.next else {
        unreachable!()
    };
    assert!(
        choices.iter().all(|c| c.ready),
        "every choice generated ahead: {choices:?}"
    );

    assert_eq!(e.advance(&play, Some(1)).unwrap(), Advance::Moved);
    let (taken, others) = e.with_graph(|g| {
        let taken = g.follow(&b.id, 1, None).unwrap().pending;
        let others: Vec<bool> = (0..choices.len())
            .filter(|i| *i != 1)
            .map(|i| g.follow(&b.id, i, None).unwrap().pending)
            .collect();
        (taken, others)
    });
    assert!(!taken);
    assert!(others.iter().all(|p| *p));

    // Export leaves pending work out and reports the choices nobody took.
    let mut buf = std::io::Cursor::new(Vec::new());
    let (_, report) = e.export(&inst, "share", &mut buf).unwrap();
    assert!(report.pending_dropped > 0);
    assert!(report.untaken.iter().any(|(id, _)| id == &b.id));
    let mut bundle =
        yarner_format::bundle::Bundle::open(std::io::Cursor::new(buf.into_inner()), "share")
            .unwrap();
    let mut g = yarner_engine::model::Graph::default();
    for r in bundle.records().unwrap() {
        g.apply(serde_json::from_slice(&r).unwrap());
    }
    assert!(g.beats.values().all(|b| !b.pending));
    for m in g.media.values().filter(|m| m.blob.is_some()) {
        assert!(bundle.read_blob(m.blob.as_ref().unwrap()).is_ok());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rerun_fork_and_staleness() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    let play = e.new_play("p").unwrap();
    for _ in 0..4 {
        to_choice(&e, &play).await;
        while e.advance(&play, Some(0)).unwrap() == Advance::Waiting {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    settle(&e).await;
    let v = e.play_view(&play).unwrap();
    let cur = v.beat.clone().unwrap();
    assert!(!cur.stale);

    // Re-run in place with a direction for this beat only.
    e.rerun(&play, Some(("make it rain".into(), Scope::Here)))
        .unwrap();
    settle(&e).await;
    let after = e.play_view(&play).unwrap().beat.unwrap();
    assert_eq!(after.id, cur.id);
    assert_eq!(after.epoch, cur.epoch + 1);
    assert!(!after.stale, "re-run uses the latest revisions");

    // Fork: a second version behind the same exit, and the play moves onto it.
    e.fork(&play, None).unwrap();
    settle(&e).await;
    let forked = e.play_view(&play).unwrap().beat.unwrap();
    assert_ne!(forked.id, cur.id);
    assert_eq!(forked.versions.len(), 2);
    e.switch_version(&play, &cur.id).unwrap();
    assert_eq!(e.play_view(&play).unwrap().beat.unwrap().id, cur.id);

    // A "from here" edit at an earlier beat makes the current beat stale.
    let path = e.plays().into_iter().find(|p| p.id == play).unwrap().path;
    let earlier = path[1].beat.clone();
    e.set_prompt(
        STORY_PROMPT,
        "Now it is a comedy.",
        Some(earlier),
        Scope::FromHere,
    )
    .unwrap();
    assert!(e.play_view(&play).unwrap().beat.unwrap().stale);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saves_load_back_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::with_garbage(Timing::instant(), 3));
    setup(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(8)).await;
    assert_eq!(r.beats, 8, "invalid replies are retried");
    let save = e.save(&play, "slot 1", Some(1), None).unwrap();
    let depth = e.play_view(&play).unwrap().depth;
    playtest::run(&e, &play, &fast_player(3)).await;
    e.back(&play).unwrap();
    assert_eq!(e.play_view(&play).unwrap().depth, depth + 2);
    e.load(&save, &play).unwrap();
    assert_eq!(e.play_view(&play).unwrap().depth, depth);
    let other = e.new_play("second run").unwrap();
    e.load(&save, &other).unwrap();
    assert_eq!(e.play_view(&other).unwrap().depth, depth);
    assert_eq!(e.saves().len(), 1);
    // Saving to a taken slot replaces what was there; another slot adds one.
    let again = e.save(&other, "", Some(1), None).unwrap();
    assert_eq!(e.saves().len(), 1);
    assert_eq!(e.saves()[0].id, again);
    e.save(&other, "", Some(2), None).unwrap();
    assert_eq!(e.saves().len(), 2);
    // Saved on a beat's last line: a decision's buttons are kept for the save screen;
    // on an earlier line they weren't on screen yet. Walk until a decision is up.
    let mut at = None;
    for _ in 0..40 {
        let beat = e.play_view(&other).unwrap().beat.unwrap().id;
        let (labels, n) = e.with_graph(|g| {
            let b = &g.beats[&beat];
            let labels: Vec<String> = match &b.next {
                Next::Choices { choices } => choices.iter().map(|c| c.label.clone()).collect(),
                _ => Vec::new(),
            };
            (labels, b.lines.len() as u32)
        });
        if !labels.is_empty() {
            at = Some((labels, n));
            break;
        }
        playtest::run(&e, &other, &fast_player(1)).await;
    }
    let (labels, n) = at.expect("a decision comes up");
    let id = e.save(&other, "", Some(3), Some(n + 5)).unwrap();
    let s = e.saves().into_iter().find(|s| s.id == id).unwrap();
    assert_eq!(s.line, Some(n - 1), "the line is kept, within the beat");
    assert_eq!(s.choices, labels, "the decision is on the save screen");
    if n > 1 {
        let id = e.save(&other, "", Some(4), Some(0)).unwrap();
        let s = e.saves().into_iter().find(|s| s.id == id).unwrap();
        assert!(s.choices.is_empty(), "no buttons before the last line");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejoins_happen_and_graph_stays_a_dag() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    e.set_depth(3);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(40)).await;
    settle(&e).await;
    let rejoins = e.with_graph(|g| {
        g.beats
            .values()
            .filter(|b| matches!(b.next, Next::Rejoin { .. }))
            .count()
    });
    // Rare by design now (a branch has its own beats first; 2026-11-21 alpha), so none
    // is fine; what must hold is that the graph never loops.
    // No cycles: a DFS never meets a beat already on its stack.
    e.with_graph(|g| {
        fn dfs(g: &yarner_engine::model::Graph, id: &str, stack: &mut Vec<String>) {
            assert!(!stack.iter().any(|s| s == id), "cycle through {id}");
            stack.push(id.to_string());
            let b = &g.beats[id];
            for exit in 0..b.next.exits() {
                for v in g.versions(id, exit).to_vec() {
                    dfs(g, &v, stack);
                }
            }
            stack.pop();
        }
        dfs(g, g.root.as_ref().unwrap(), &mut vec![]);
    });
    let map = e.map(Some(&play));
    assert!(map.nodes.iter().any(|n| n.on_path));
    // Any rejoin there is shows on the map as one.
    assert_eq!(map.edges.iter().any(|x| x.rejoin), rejoins > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_on_disk_in_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", br#"{"title":"The Drowned Lamp"}"#)
        .unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    let play = e.new_play("my secret run").unwrap();
    playtest::run(&e, &play, &fast_player(12)).await;
    e.save(&play, "before the storm", None, None).unwrap();
    settle(&e).await;
    let mut secrets: Vec<String> = vec![
        STORY.into(),
        DESCR.into(),
        "moody watercolor".into(),
        "The Drowned Lamp".into(),
        "my secret run".into(),
        "before the storm".into(),
    ];
    e.with_graph(|g| {
        for b in g.beats.values() {
            secrets.extend(b.lines.iter().map(|l| l.text.clone()));
        }
        for m in g.media.values() {
            secrets.push(m.prompt.clone());
            // What's spoken, and the sounds asked for, are story too.
            secrets.extend(m.audio.iter().map(|a| a.words.clone()));
        }
    });
    secrets.retain(|s| !s.is_empty());
    let mut files = vec![];
    let mut stack = vec![dir.path().to_path_buf()];
    while let Some(d) = stack.pop() {
        for x in std::fs::read_dir(d).unwrap() {
            let p = x.unwrap().path();
            if p.is_dir() {
                stack.push(p)
            } else {
                files.push(p)
            }
        }
    }
    // config, header, graph log, plays log and at least one picture.
    assert!(files.len() >= 5, "{files:?}");
    for f in files {
        let bytes = std::fs::read(&f).unwrap();
        for s in &secrets {
            let s = s.as_bytes();
            for w in s.windows(12.min(s.len())).step_by(4) {
                assert!(
                    !bytes.windows(w.len()).any(|x| x == w),
                    "{:?} in {}",
                    String::from_utf8_lossy(w),
                    f.display()
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_send_only_new_turns_and_survive_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(20)).await;
    assert_eq!(r.beats, 20);
    settle(&e).await;
    let st = mock.stats();
    assert!(st.resumed > st.roots, "most beats continue a node: {st:?}");
    // Requests carry ~1-2 messages, not the whole conversation.
    assert!(
        st.messages_sent as f64 / st.text_jobs as f64 <= 2.5,
        "{st:?}"
    );
    // Every beat records the chat it came from.
    e.with_graph(|g| {
        assert!(
            g.beats
                .values()
                .all(|b| b.turn.is_some() && b.reply.is_some())
        );
        assert!(g.beats.values().any(|b| !b.seg_start));
    });
    // The engine's own record agrees: most calls resumed a node and sent little.
    // (Beats only: the shot step's turns are text jobs too, not counted there.)
    let ts = e.text_stats();
    assert_eq!(ts.len(), (st.text_jobs - st.shots) as usize, "{st:?}");
    let resumed = ts.iter().filter(|s| s.resumed).count();
    assert!(resumed <= st.resumed as usize, "{st:?}");
    assert!(
        resumed * 10 >= ts.len() * 7,
        "{resumed} of {}: {st:?}",
        ts.len()
    );
    assert!(ts.iter().filter(|s| s.resumed).all(|s| s.messages <= 2));

    // Evictions: generation keeps going by resending the conversation.
    mock.evict_sessions_every(3);
    let r = playtest::run(&e, &play, &fast_player(12)).await;
    assert_eq!(r.beats, 12);
    assert!(mock.stats().gone > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segment_one_is_the_classic_full_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        segment: 1,
        sessions: false,
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(8)).await;
    assert_eq!(r.beats, 8);
    let st = mock.stats();
    assert_eq!(st.resumed + st.roots, 0);
    assert!(e.text_stats().iter().all(|s| !s.resumed && s.messages >= 2));
    e.with_graph(|g| assert!(g.beats.values().all(|b| b.seg_start)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_player_sees_lines_as_they_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let slow = Timing {
        text: Duration::from_millis(600),
        image: Duration::from_millis(50),
        video: Duration::from_millis(50),
        audio: Duration::from_millis(50),
    };
    let e = open(&inst, "pw", MockThinfer::new(slow));
    setup(&e);
    let play = e.new_play("p").unwrap();
    assert_eq!(e.advance(&play, None).unwrap(), Advance::Waiting);
    // Poll while the opening beat is written: a draft shows up before the beat exists.
    let mut drafted: Option<Vec<yarner_engine::model::Line>> = None;
    for _ in 0..400 {
        if e.advance(&play, None).unwrap() == Advance::Moved {
            break;
        }
        if let Some(d) = e.play_view(&play).unwrap().draft
            && !d.is_empty()
        {
            drafted = Some(d);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let drafted = drafted.expect("a draft before the beat was done");
    let beat = e.play_view(&play).unwrap().beat.unwrap();
    assert_eq!(
        beat.lines[..drafted.len()],
        drafted[..],
        "the draft is the beat's start"
    );
    assert!(e.play_view(&play).unwrap().draft.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_generation_withdraws_its_draft_and_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let slow = Timing {
        text: Duration::from_millis(300),
        image: Duration::from_millis(20),
        video: Duration::from_millis(20),
        audio: Duration::from_millis(20),
    };
    let mock = MockThinfer::new(slow);
    // The opening beat's first take crashes after streaming its lines.
    mock.fail_text_every(1);
    // No tools: a lookup reply has no lines to stream (the mock's choices follow the
    // prompt's words, so whether it looks something up first changes with the wording).
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    let mut events = e.subscribe();
    setup(&e);
    let play = e.new_play("p").unwrap();
    assert_eq!(e.advance(&play, None).unwrap(), Advance::Waiting);
    let mut saw_draft = false;
    let mut withdrawn = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !withdrawn {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("draft events")
            .unwrap();
        if let yarner_engine::Event::Draft { .. } = ev {
            match e.play_view(&play).unwrap().draft {
                Some(d) if !d.is_empty() => saw_draft = true,
                _ if saw_draft => withdrawn = true,
                _ => {}
            }
        }
    }
    assert_eq!(e.advance(&play, None).unwrap(), Advance::Waiting);
    // Let it succeed: the waiting player still gets the beat.
    mock.fail_text_every(0);
    for _ in 0..400 {
        if e.advance(&play, None).unwrap() == Advance::Moved {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(e.play_view(&play).unwrap().beat.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_story_keeps_generating_in_a_fresh_session() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    let play = e.new_play("p").unwrap();
    assert_eq!(playtest::run(&e, &play, &fast_player(10)).await.beats, 10);
    settle(&e).await;
    let (cur, _) = e
        .export(&inst, "bundle pass", std::io::Cursor::new(Vec::new()))
        .unwrap();
    let exported = e.with_graph(|g| g.beats.values().filter(|b| !b.pending).count());
    e.shutdown().await;

    // A different machine: its own install, a new passphrase, a cold backend.
    let dir2 = tempfile::tempdir().unwrap();
    let inst2 = install(dir2.path());
    let story = yarner_format::bundle::import(
        &inst2,
        std::io::Cursor::new(cur.into_inner()),
        "bundle pass",
        "new pass",
    )
    .unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e2 = Engine::open(story, mock.clone(), Config::default()).unwrap();
    e2.with_graph(|g| assert!(g.beats.len() >= exported));
    let play = e2.new_play("q").unwrap();
    // Well past what was exported: generation picks up from imported beats.
    let r = playtest::run(&e2, &play, &fast_player(exported + 12)).await;
    assert_eq!(r.beats, exported + 12);
    let st = mock.stats();
    assert!(
        st.roots >= 1,
        "a fresh session starts from the full chat: {st:?}"
    );
    assert!(st.resumed > 0, "then continues it: {st:?}");
    assert_eq!(st.gone, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_opening_beat_gets_its_picture() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    // Default config, as the server runs it.
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = Engine::open(story, mock.clone(), Config::default()).unwrap();
    setup(&e);
    let png = yarner_testkit::png::placeholder(16, 16, 1);
    let m = e
        .upload_media(&png, "image/png", "Mira", vec!["Mira".into()])
        .unwrap();
    let refs = vec![yarner_engine::model::RefImage {
        use_for: String::new(),
        media: m,
        caption: "Mira".into(),
    }];
    e.put_object(None, "Tomas", "A fisherman.", refs, false, None, None)
        .unwrap();
    let play = e.new_play("p").unwrap();
    let _ = e.advance(&play, None).unwrap();
    for _ in 0..400 {
        let v = e.play_view(&play).unwrap();
        if let Some(b) = &v.beat
            && let Some(s) = &b.shows
            && s.status == yarner_engine::model::MediaStatus::Ready
            // A picture painted for it (it may reuse one it looked up instead: play on).
            && !mock.stats().media_prompts.is_empty()
        {
            // Sent in the story's style, whatever the LLM's prompt said, with the
            // author's description of whoever it names (written out by the shot step,
            // or added after the beat's own prompt when that step falls back).
            let sent = mock.stats().media_prompts;
            assert!(
                sent.iter().any(|p| p.ends_with("Style: moody watercolor")),
                "{sent:?}"
            );
            assert!(
                sent.iter()
                    .any(|p| p.contains(DESCR) || p.contains("A fisherman.")),
                "{sent:?}"
            );
            return;
        }
        let _ = e.advance(&play, Some(0));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let v = e.play_view(&play).unwrap();
    panic!(
        "no picture on the opening beat: {:?}",
        v.beat.map(|b| b.shows)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn choices_are_spaced_out() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let e = Engine::open(
        story,
        MockThinfer::new(Timing::instant()),
        Config::default(),
    )
    .unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(40)).await;
    assert_eq!(r.beats, 40);
    assert!(r.choices > 0);
    let path = e.plays().into_iter().find(|p| p.id == play).unwrap().path;
    e.with_graph(|g| {
        let choice: Vec<bool> = path
            .iter()
            .map(|h| matches!(g.beats[&h.beat].next, Next::Choices { .. }))
            .collect();
        for (i, w) in choice
            .windows(yarner_engine::context::CHOICE_GAP + 1)
            .enumerate()
        {
            assert!(
                w.iter().filter(|c| **c).count() <= 1,
                "choices too close at beat {i}: {choice:?}"
            );
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_opening_is_ready_before_start_and_follows_setup_changes() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        prefetch_after_secs: Some(0.0),
        ..Config::default()
    };
    let e = Engine::open(story, MockThinfer::new(Timing::instant()), cfg).unwrap();
    setup(&e);
    settle(&e).await;
    // Written ahead, unseen, with its picture made.
    let (root, epoch) = e.with_graph(|g| {
        let r = g.root.clone().expect("opening written during setup");
        let b = &g.beats[&r];
        assert!(b.pending);
        if let yarner_engine::model::Show::New { media } = &b.show {
            assert_eq!(
                g.media[media].status,
                yarner_engine::model::MediaStatus::Ready
            );
        }
        (r, b.epoch)
    });

    // The author changes the story: the unseen opening is rewritten.
    e.set_prompt(
        STORY_PROMPT,
        "A different story entirely.",
        None,
        Scope::FromHere,
    )
    .unwrap();
    settle(&e).await;
    e.with_graph(|g| {
        let b = &g.beats[&root];
        assert!(b.epoch > epoch, "rewritten after the setup changed");
        assert!(b.pending);
        assert!(!yarner_engine::model::is_stale(
            g,
            &[yarner_engine::model::Hop {
                beat: root.clone(),
                exit: None
            }]
        ));
    });

    // Start: the opening is there at once.
    let play = e.new_play("p").unwrap();
    assert_eq!(e.advance(&play, None).unwrap(), Advance::Moved);
    assert_eq!(e.play_view(&play).unwrap().beat.unwrap().id, root);
}

/// A screen comes whole: no beat goes on screen before its picture (or video) is made,
/// and the way there says it's painting or filming.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_screen_is_shown_only_with_its_picture() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(5),
        image: Duration::from_millis(120),
        video: Duration::from_millis(500),
        audio: Duration::from_millis(500),
    });
    mock.video_every(2);
    let e = open(&inst, "pw", mock);
    setup(&e);
    let play = e.new_play("p").unwrap();
    let (mut beats, mut saw_painting, mut saw_filming, mut waited) = (0, false, false, false);
    let mut videos_shown = 0;
    let mut progress: Vec<f32> = vec![];
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while beats < 30 || (videos_shown == 0 && beats < 150) {
        assert!(
            std::time::Instant::now() < deadline,
            "stuck after {beats} beats"
        );
        let v = e.play_view(&play).unwrap();
        if let Some(b) = &v.beat {
            let m = b.screen.as_ref().expect("never a blank screen");
            assert_eq!(
                m.status,
                yarner_engine::model::MediaStatus::Ready,
                "beat {beats} went on screen before its picture was made: {m:?}"
            );
            if b.shows
                .as_ref()
                .is_some_and(|s| s.kind == yarner_engine::model::MediaKind::Video)
            {
                videos_shown += 1;
            }
            match &b.next {
                NextView::Continue {
                    painting,
                    filming,
                    progress: p,
                    ..
                } => {
                    saw_painting |= painting;
                    saw_filming |= filming;
                    progress.extend(*p);
                }
                NextView::Choices { choices } => {
                    saw_painting |= choices.iter().any(|c| c.painting);
                    saw_filming |= choices.iter().any(|c| c.filming);
                    progress.extend(choices.iter().filter_map(|c| c.progress));
                }
                NextView::End => break,
            }
        }
        let exit = match v.beat.as_ref().map(|b| &b.next) {
            Some(NextView::Choices { choices }) => Some(choices.len() - 1),
            _ => None,
        };
        let unmade = v.beat.as_ref().is_some_and(|b| match &b.next {
            NextView::Continue { painting, .. } => *painting,
            NextView::Choices { choices } => choices.last().is_some_and(|c| c.painting),
            NextView::End => false,
        });
        match e.advance(&play, exit).unwrap() {
            Advance::Moved => beats += 1,
            Advance::Waiting => {
                waited |= unmade;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Advance::Ended => break,
        }
    }
    assert!(beats >= 10, "played {beats}");
    assert!(
        saw_painting && waited,
        "a next screen should have waited on its picture"
    );
    assert!(
        progress.iter().any(|p| *p > 0.0),
        "a picture being made should report how far it is: {progress:?}"
    );
    assert!(saw_filming, "a next screen should have said it's filming");
    assert!(
        videos_shown > 0,
        "a video beat should have gone on screen with its video"
    );
}

/// The opening's schema can't keep a blank screen: its first beat always paints.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_opening_never_starts_on_a_blank_screen() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(5),
        image: Duration::from_millis(200),
        video: Duration::ZERO,
        audio: Duration::ZERO,
    });
    let e = open(&inst, "pw", mock);
    setup(&e);
    let play = e.new_play("p").unwrap();
    loop {
        match e.advance(&play, None).unwrap() {
            Advance::Moved => break,
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    let b = e.play_view(&play).unwrap().beat.unwrap();
    let m = b.screen.expect("the opening has a picture");
    assert_eq!(m.status, yarner_engine::model::MediaStatus::Ready);
}

/// Video the story asks for is made during play, even for a reader who never pauses (it
/// used to wait for idle time that never came).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn videos_are_made_while_reading_and_seen() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(20),
        image: Duration::from_millis(60),
        video: Duration::from_millis(600),
        audio: Duration::from_millis(600),
    });
    mock.video_every(2);
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(40)).await;
    assert!(!r.gave_up, "{r:?}");
    let v = e.play_view(&play).unwrap();
    let (videos, made) = e.with_graph(|g| {
        let ids: Vec<String> = v.history.iter().map(|h| h.beat.clone()).collect();
        let (mut videos, mut made) = (0, 0);
        for (i, id) in ids.iter().enumerate() {
            let Some(yarner_engine::model::Show::New { media }) = g.beats.get(id).map(|b| &b.show)
            else {
                continue;
            };
            let m = &g.media[media];
            if m.kind != yarner_engine::model::MediaKind::Video {
                continue;
            }
            // Only videos well behind the reader: the ones just ahead may still be running.
            if i + 4 >= ids.len() {
                continue;
            }
            videos += 1;
            if m.status == yarner_engine::model::MediaStatus::Ready {
                made += 1;
            }
        }
        (videos, made)
    });
    assert!(videos > 0, "the run should have asked for video");
    assert_eq!(
        made, videos,
        "every video behind the reader was made during play"
    );
    // The model's refs reach the video job (how they're used is the model's call).
    let s = mock.stats();
    assert!(s.videos > 0 && s.videos_with_refs > 0, "{s:?}");
    // The map marks the screens that play a video, and only those: one held on a video's
    // last frame shows that frame unmarked.
    let map = e.map(Some(&play));
    e.with_graph(|g| {
        use yarner_engine::model::{MediaKind, Show};
        let mut marked = 0;
        for n in &map.nodes {
            let plays = match &g.beats[&n.id].show {
                Show::New { media } | Show::Reuse { media } => {
                    g.media[media].kind == MediaKind::Video
                }
                Show::Keep => false,
            };
            assert_eq!(n.video, plays, "beat {}", n.id);
            marked += usize::from(n.video);
        }
        assert!(marked > 0, "some screen on the map is a video");
    });
}

/// A picture that fails is tried again (it's what the story asked for); after a few
/// failures it's given up on and the screen keeps the picture before, so play goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_picture_is_retried_then_given_up_on() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    // The opening's picture fails twice, then works.
    mock.fail_media(2);
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while e.advance(&play, None).unwrap() != Advance::Moved {
        assert!(
            std::time::Instant::now() < deadline,
            "the opening never showed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let shows = e.play_view(&play).unwrap().beat.unwrap().shows.unwrap();
    assert_eq!(
        shows.status,
        yarner_engine::model::MediaStatus::Ready,
        "painted on its third try"
    );
    // Every picture from now on fails for good: play still goes on.
    mock.fail_media(1000);
    let failed = || {
        e.with_graph(|g| {
            g.media
                .values()
                .filter(|m| m.status == yarner_engine::model::MediaStatus::Failed)
                .count()
        })
    };
    // Play on until a new picture is asked for, then give its retries (with backoff)
    // time to run out: a deadline on the outcome, not a count of beats.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while failed() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a picture failing every time is given up on"
        );
        let r = playtest::run(&e, &play, &fast_player(3)).await;
        assert!(!r.gave_up, "{r:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// An unreachable backend isn't hammered, and the story picks up once it's back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_backend_is_retried_gently_and_recovered_from() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.set_down(true);
    let e = open(&inst, "pw", mock.clone());
    let mut events = e.subscribe();
    setup(&e);
    let play = e.new_play("p").unwrap();
    let _ = e.advance(&play, None).unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let mut errors = 0;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, yarner_engine::Event::Error { .. }) {
            errors += 1;
        }
    }
    // Backoff: tried at about 0, 2 and 6 s, not every half second; said once.
    assert!(errors <= 2, "{errors} error events in 5 s");
    mock.set_down(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while e.advance(&play, None).unwrap() != Advance::Moved {
        assert!(
            std::time::Instant::now() < deadline,
            "never recovered after the backend came back"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Text a reader waits on is sent to cut in (the backend pauses a picture or video in
/// the way, which resumes after): nothing is thrown away and every picture still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_text_cuts_in_and_nothing_is_thrown_away() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(30),
        image: Duration::from_millis(400),
        video: Duration::from_millis(400),
        audio: Duration::from_millis(400),
    });
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(15)).await;
    assert!(!r.gave_up, "{r:?}");
    let s = mock.stats();
    assert!(s.preempts > 0, "waiting text should cut in: {s:?}");
    // Pictures pause; only text written ahead in the way of a reader's next screen is
    // dropped (and written again later).
    assert_eq!(s.cancelled_media, 0, "no picture thrown away: {s:?}");
    settle(&e).await;
    let v = e.play_view(&play).unwrap();
    assert_eq!(
        v.beat.and_then(|b| b.screen).map(|m| m.status),
        Some(yarner_engine::model::MediaStatus::Ready)
    );
}

/// A job that cuts in pauses the picture being painted; the picture finishes after it,
/// with nothing lost (the mock does what thinfer does).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_cuts_in_pauses_a_picture_which_then_finishes() {
    use yarner_engine::backend::{Backend, ImageRequest, Job, TextRequest};
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(20),
        image: Duration::from_millis(500),
        video: Duration::ZERO,
        audio: Duration::ZERO,
    });
    let picture = mock
        .submit(
            Job::Image(ImageRequest {
                model: "qwen-image-2.1-turbo".into(),
                prompt: "a lighthouse".into(),
                width: 64,
                height: 64,
                refs: vec![],
                seed: None,
                loras: vec![],
            }),
            100,
            false,
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let t0 = std::time::Instant::now();
    let text = mock
        .submit(
            Job::Text(TextRequest {
                messages: vec![yarner_engine::backend::Message::new(
                    "user",
                    "Begin the story.",
                )],
                ..TextRequest::default()
            }),
            1000,
            true,
        )
        .await
        .unwrap();
    text.done.await.unwrap();
    // Text didn't wait for the picture's remaining ~350 ms.
    assert!(
        t0.elapsed() < Duration::from_millis(250),
        "{:?}",
        t0.elapsed()
    );
    picture.done.await.unwrap();
    let s = mock.stats();
    assert_eq!((s.paused, s.cancelled), (1, 0), "{s:?}");
}

/// A character changed "from here" reaches what comes next: beats written ahead (unseen)
/// are rewritten with the change before they're shown, and pictures use the new
/// description.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_from_here_reaches_unseen_beats_and_pictures() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(4)).await;
    settle(&e).await;
    // Mira changes from the beat on screen on.
    let here = e.play_view(&play).unwrap().beat.unwrap().id;
    let mira = e.with_graph(|g| {
        g.objects
            .iter()
            .find(|o| o.name == "Mira")
            .map(|o| o.id.clone())
            .unwrap()
    });
    const NEW: &str = "Mira, now silver-haired, in a red storm cloak.";
    let before = mock.stats().media_prompts.len();
    e.put_object(
        Some(&mira),
        "Mira",
        NEW,
        vec![],
        false,
        None,
        Some(here.clone()),
    )
    .unwrap();
    // A new picture on every screen from here (the test is that pictures pick up the
    // change; waiting on the mock to happen to paint one was flaky).
    let mut s = e.setup_view(Some(&play)).settings;
    s.picture_gap = 0;
    s.picture_max = 1;
    e.set_settings(s, None).unwrap();
    // Play on (until a picture of the new Mira comes up): nothing shown from here on
    // was written for the old Mira.
    for _ in 0..80 {
        let r = playtest::run(&e, &play, &fast_player(1)).await;
        assert!(!r.gave_up, "{r:?}");
        let v = e.play_view(&play).unwrap();
        let b = v.beat.unwrap();
        assert!(
            !b.stale,
            "beat {} on screen was written before the change",
            b.id
        );
        if mock.stats().media_prompts[before..]
            .iter()
            .any(|p| p.contains(NEW))
        {
            break;
        }
    }
    settle(&e).await;
    // Pictures pick up the new Mira (pictures for other branches, or for beats before
    // the change, rightly keep the old one: the change is from here on).
    let sent = mock.stats().media_prompts;
    assert!(
        sent[before..].iter().any(|p| p.contains(NEW)),
        "a picture after the change should use the new Mira: {:?}",
        &sent[before..]
    );
}

/// At a decision the reader can write their own choice: it's added (the offered ones and
/// what was written for them stay), and the story goes on from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_choice_of_ones_own_is_added_and_taken() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    let play = e.new_play("p").unwrap();
    to_choice(&e, &play).await;
    settle(&e).await;
    let b = e.play_view(&play).unwrap().beat.unwrap();
    assert!(b.own_choices);
    let NextView::Choices { choices } = &b.next else {
        unreachable!()
    };
    let offered = choices.len();
    let before: Vec<Vec<String>> = (0..offered)
        .map(|i| e.with_graph(|g| g.versions(&b.id, i).to_vec()))
        .collect();
    let exit = e
        .add_choice(&play, "  Climb the lamp housing and wave the lantern  ")
        .unwrap();
    assert_eq!(exit, offered, "added after the offered ones");
    // The same words again pick the same choice.
    assert_eq!(
        e.add_choice(&play, "climb the lamp housing and wave the lantern")
            .unwrap(),
        exit
    );
    let after: Vec<Vec<String>> = (0..offered)
        .map(|i| e.with_graph(|g| g.versions(&b.id, i).to_vec()))
        .collect();
    assert_eq!(
        before, after,
        "what was written for the offered choices stays"
    );
    while e.advance(&play, Some(exit)).unwrap() == Advance::Waiting {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let v = e.play_view(&play).unwrap();
    assert_eq!(
        v.history
            .iter()
            .rev()
            .find_map(|h| h.chose.clone())
            .as_deref(),
        Some("Climb the lamp housing and wave the lantern")
    );
}

/// An ending isn't the end if the reader wants more: the beat goes on (its words
/// unchanged) and the next one is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_going_past_an_ending() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    // The storyteller ends the story at beat 5.
    mock.end_at(5);
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    // A story allowed to end this soon.
    e.set_settings(
        yarner_engine::model::Settings {
            min_beats: 0,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let play = e.new_play("p").unwrap();
    // Play until the ending.
    let mut ended = false;
    for _ in 0..400 {
        match e.advance(&play, Some(0)).unwrap_or(Advance::Waiting) {
            Advance::Ended => {
                ended = true;
                break;
            }
            Advance::Waiting => tokio::time::sleep(Duration::from_millis(2)).await,
            Advance::Moved => {}
        }
    }
    assert!(ended, "the story should have ended");
    // More than the ending: the storyteller goes on.
    mock.end_at(0);
    let b = e.play_view(&play).unwrap().beat.unwrap();
    e.keep_going(&play).unwrap();
    let again = e.play_view(&play).unwrap().beat.unwrap();
    assert_eq!(again.lines, b.lines, "its words are unchanged");
    assert!(matches!(again.next, NextView::Continue { .. }));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while e.advance(&play, None).unwrap() != Advance::Moved {
        assert!(
            std::time::Instant::now() < deadline,
            "nothing written after the ending"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// "Take another direction next": it applies to the beat right after this one only (a
/// beat already written ahead is rewritten with it), not to anything further on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direction_for_next_applies_to_the_next_beat_only() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let e = open(&inst, "pw", MockThinfer::new(Timing::instant()));
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(3)).await;
    settle(&e).await;
    let here = e.play_view(&play).unwrap().beat.unwrap().id;
    let rev = e
        .set_prompt(
            &format!("next:{here}"),
            "a stranger knocks at the door",
            Some(here.clone()),
            Scope::Next,
        )
        .unwrap();
    // On to the next beat and one more.
    let mut seen = vec![];
    for _ in 0..2 {
        loop {
            let b = e.play_view(&play).unwrap().beat.unwrap();
            let exit = match &b.next {
                NextView::Choices { .. } => Some(0),
                _ => None,
            };
            match e.advance(&play, exit).unwrap() {
                Advance::Moved => break,
                Advance::Waiting => tokio::time::sleep(Duration::from_millis(5)).await,
                Advance::Ended => panic!("ended"),
            }
        }
        let id = e.play_view(&play).unwrap().beat.unwrap().id;
        seen.push(e.with_graph(|g| g.beats[&id].used.contains(&rev)));
    }
    assert_eq!(
        seen,
        [true, false],
        "the next beat follows it; the one after doesn't"
    );
}

/// A setup change saved while the opening is written for a reader who already pressed
/// Start doesn't send them round again: the opening written since they asked is theirs
/// (2026-11-21 alpha: rewritten 4 times, 86 s before the story began).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edit_during_the_opening_doesnt_restart_it() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let slow = Timing {
        text: Duration::from_millis(400),
        image: Duration::from_millis(10),
        video: Duration::from_millis(10),
        audio: Duration::from_millis(10),
    };
    let mock = MockThinfer::new(slow);
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        prefetch_after_secs: None,
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    assert_eq!(e.advance(&play, None).unwrap(), Advance::Waiting);
    // While it's being written, the author saves a change to what the story is (one that
    // makes the words stale; the style only repaints).
    tokio::time::sleep(Duration::from_millis(150)).await;
    e.set_prompt(
        STORY_PROMPT,
        "A lighthouse keeper on a storm-cut island.",
        None,
        Scope::FromHere,
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while e.advance(&play, None).unwrap() != Advance::Moved {
        assert!(
            std::time::Instant::now() < deadline,
            "the story never began"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Its first take, not a rewrite (a rewrite is a new epoch of the beat).
    let b = e.play_view(&play).unwrap().beat.unwrap();
    assert_eq!(b.epoch, 0, "the opening was rewritten");
}

/// A steer for pictures goes word for word into every picture made from where it was
/// given, and doesn't send text written ahead round again (it changes no beat's words).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_picture_steer_reaches_every_picture_word_for_word() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    let mut events = e.subscribe();
    setup(&e);
    e.set_prompt(
        "pictures",
        "she looks straight at the viewer",
        None,
        Scope::FromHere,
    )
    .unwrap();
    let play = e.new_play("p").unwrap();
    let _ = e.advance(&play, None);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // Play on until a picture has been made, moving whenever something lands.
    while mock.stats().media_prompts.is_empty() {
        let _ = e.advance(&play, Some(0));
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no picture was made");
    }
    let sent = mock.stats().media_prompts;
    assert!(
        sent.iter()
            .all(|p| p.starts_with("she looks straight at the viewer\n\n")),
        "{sent:?}"
    );
    // It isn't part of what a beat's words depend on.
    let v = e.play_view(&play).unwrap();
    assert!(!v.beat.map(|b| b.stale).unwrap_or(false));
}

/// A new branch plays out its own consequences: right after a fork it offers no way
/// back into the line that was taken (2026-11-21 alpha: every untaken choice flowed
/// straight back into it, leaving nothing to go back to).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_untaken_choice_doesnt_fall_straight_back_into_the_taken_line() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    // Play well past a few decisions, taking the first choice each time, so every
    // other choice is written ahead as an untaken branch.
    playtest::run(&e, &play, &fast_player(12)).await;
    settle(&e).await;
    let bad = e.with_graph(|g| {
        let mut bad = vec![];
        for b in g.beats.values() {
            let Next::Choices { choices } = &b.next else {
                continue;
            };
            for exit in 0..choices.len() {
                // The first beats of each branch: none rejoins anything.
                let mut cur = g.follow(&b.id, exit, None);
                for _ in 0..2 {
                    let Some(c) = cur else { break };
                    if matches!(c.next, Next::Rejoin { .. }) {
                        bad.push(c.id.clone());
                    }
                    cur = g.follow(&c.id, 0, None);
                }
            }
        }
        bad
    });
    assert!(
        bad.is_empty(),
        "branches rejoined right after their fork: {bad:?}"
    );
}

/// A new shape only repaints: words written ahead stay (no rewrite), and pictures
/// nobody has seen yet are made again at the new size.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_shape_repaints_without_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        prefetch_after_secs: Some(0.0),
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    let mut events = e.subscribe();
    setup(&e);
    // The opening is written ahead (nobody has started) with its picture.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let painted = |e: &Engine| {
        e.with_graph(|g| {
            let r = g.root.clone()?;
            let b = &g.beats[&r];
            let yarner_engine::model::Show::New { media } = &b.show else {
                return None;
            };
            let m = &g.media[media];
            (m.status == yarner_engine::model::MediaStatus::Ready)
                .then_some((r, b.epoch, m.width, m.height))
        })
    };
    while painted(&e).is_none() {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the opening was never painted");
    }
    let (root, epoch, w, h) = painted(&e).unwrap();
    assert_eq!((w, h), (832, 480));
    e.set_settings(
        yarner_engine::model::Settings {
            width: 480,
            height: 832,
            orientation: yarner_engine::model::Orientation::Portrait,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    while painted(&e).is_none_or(|(_, _, w, _)| w != 480) {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the opening's picture was never made again at the new size");
    }
    let (root2, epoch2, _, _) = painted(&e).unwrap();
    assert_eq!(
        (root2, epoch2),
        (root, epoch),
        "its words weren't rewritten"
    );
}

/// The ladder: work higher up interrupts work lower down (what a reader waits on, then
/// their next screens, then on down), and never work on its own rung: siblings take
/// turns (2026-11-21 alpha: a choice's video paused again and again by its siblings'
/// text took 522 s). 2026-11-22 alpha: a decision's screens queued behind a picture for
/// far ahead; only what a reader waited on could cut in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn higher_work_interrupts_lower_never_its_own_rung() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    // Real-world proportions, scaled down: text 4 s, a picture 30 s, a video 200 s.
    let mock = MockThinfer::new(Timing::realistic(0.004));
    mock.video_every(3);
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    use yarner_engine::engine::prio::{self, tier};
    // Read on until a next screen has had the chance to cut in (in 1 run of 30, 14 beats
    // never gave it one); every cut-in of the whole read is checked.
    for _ in 0..4 {
        let r = playtest::run(&e, &play, &fast_player(14)).await;
        assert!(!r.gave_up, "{r:?}");
        if mock
            .stats()
            .cut_ins
            .iter()
            .any(|(by, _)| tier(*by) == tier(prio::NOW))
        {
            break;
        }
    }
    let s = mock.stats();
    for (by, over) in &s.cut_ins {
        assert!(
            tier(*by) > tier(*over),
            "{by} cut in over {over}, on the same rung or higher ({:?})",
            s.cut_ins
        );
    }
    // Next screens interrupt what's further off, not only what a reader waits on.
    assert!(
        s.cut_ins.iter().any(|(by, _)| tier(*by) == tier(prio::NOW)),
        "no next screen ever cut in: {:?}",
        s.cut_ins
    );
    assert_eq!(
        s.cancelled_media, 0,
        "no picture or video thrown away: {s:?}"
    );
}

/// The opening can be forked too: a new version of how the story begins, the old one
/// kept and switchable (2026-11-21 alpha: "Fork from here" was off on the opening).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_opening_can_be_forked_and_switched_back() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    let mut events = e.subscribe();
    setup(&e);
    let play = e.new_play("p").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while e.advance(&play, None).unwrap() != Advance::Moved {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the story never began");
    }
    let first = e.play_view(&play).unwrap().beat.unwrap();
    e.fork(&play, Some("Begin at dawn instead.".into()))
        .unwrap();
    // The playthrough moves onto the new opening once it's written.
    while e.play_view(&play).unwrap().beat.unwrap().id == first.id {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the opening was never forked");
    }
    let second = e.play_view(&play).unwrap().beat.unwrap();
    assert_eq!(second.versions, vec![first.id.clone(), second.id.clone()]);
    assert_eq!(second.version, 1);
    // The old one is still there to go back to, and the story still goes on from it.
    e.switch_version(&play, &first.id).unwrap();
    assert_eq!(e.play_view(&play).unwrap().beat.unwrap().id, first.id);
    while e.advance(&play, Some(0)).unwrap() != Advance::Moved {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("nothing after the old opening");
    }
    // Both openings survive a reload of the story (snapshot / log replay).
    e.compact().unwrap();
    assert_eq!(e.with_graph(|g| g.openings.len()), 2);
}

/// A new shape saved while the picture is being made: the picture at the old size isn't
/// kept over the request for the new one (it was, for good: nothing looks at a ready
/// picture again).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_shape_while_painting_isnt_lost() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    // Timing is what's asserted: the change has to land while the picture is made.
    let mock = MockThinfer::new(Timing {
        text: Duration::ZERO,
        image: Duration::from_millis(400),
        video: Duration::ZERO,
        audio: Duration::ZERO,
    });
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        prefetch_after_secs: Some(0.0),
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    let mut events = e.subscribe();
    setup(&e);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let opening = |e: &Engine| {
        e.with_graph(|g| {
            let b = &g.beats[g.root.as_ref()?];
            let yarner_engine::model::Show::New { media } = &b.show else {
                return None;
            };
            let m = &g.media[media];
            Some((m.status, m.width))
        })
    };
    // Written; its picture under way.
    while opening(&e).is_none() {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("the opening was never written");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        opening(&e).map(|o| o.0),
        Some(yarner_engine::model::MediaStatus::Requested)
    );
    e.set_settings(
        yarner_engine::model::Settings {
            width: 480,
            height: 832,
            orientation: yarner_engine::model::Orientation::Portrait,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    while opening(&e) != Some((yarner_engine::model::MediaStatus::Ready, 480)) {
        let _ = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("never painted at the new size: {:?}", opening(&e)));
    }
}

/// A steer makes the screens written ahead stale: while one is rewritten it says so
/// ("writing"), never "up next" with nothing to show (2026-11-22 alpha).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_screen_being_rewritten_after_a_steer_says_writing() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    // Timing is what's asserted: the rewrite has to be seen under way.
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(300),
        image: Duration::ZERO,
        video: Duration::ZERO,
        audio: Duration::ZERO,
    });
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(3)).await;
    settle(&e).await;
    let here = e.play_view(&play).unwrap().beat.unwrap().id;
    e.set_prompt(
        "steer-test",
        "Make it rain.",
        Some(here.clone()),
        Scope::FromHere,
    )
    .unwrap();
    // The reader waits on the next screen, which has to be rewritten.
    let _ = e.advance(&play, Some(0));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut saw_writing = false;
    // The rewrite on a worker (by job), from the backend's queue.
    let rewrite_running = |q: &[yarner_engine::backend::QueueEntry]| {
        q.iter()
            .find(|j| j.running && j.what.as_deref().is_some_and(|w| w.starts_with("rerun ")))
            .map(|j| j.id.clone())
    };
    while tokio::time::Instant::now() < deadline {
        let q0 = rewrite_running(&e.queue().await);
        let v = e.play_view(&play).unwrap();
        let q1 = rewrite_running(&e.queue().await);
        // On a worker the whole time the view was taken (waiting its turn, it's "up
        // next", which is right).
        let on_worker = q0.is_some() && q0 == q1;
        let Some(b) = v.beat else { break };
        if b.id != here {
            break;
        }
        let (ready, generating, line) = match &b.next {
            NextView::Continue {
                ready,
                generating,
                line,
                ..
            } => (*ready, *generating, *line),
            NextView::Choices { choices } => {
                let c = &choices[0];
                (c.ready, c.generating, c.line)
            }
            _ => break,
        };
        let rewriting = e.with_graph(|g| g.follow(&here, 0, None).is_some());
        if !ready && rewriting && generating {
            saw_writing = true;
        }
        assert!(
            ready || generating || line.is_some() || !rewriting || !on_worker,
            "a screen being rewritten showed as idle"
        );
        let _ = e.advance(&play, Some(0));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_writing, "the rewrite was never shown as writing");
}

/// "Keep generating while I'm away": once the reader is away the story is filled out wide
/// (every choice, not just the likeliest line) and deep (past `max_depth`), each screen
/// whole, and it stops at the endings (2026-11-22 alpha: it stopped ~6 beats down one line).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_generating_fills_wide_and_deep_and_stops_at_the_ends() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.end_at(9);
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        away_secs: 0.0,
        ..Config::default()
    };
    let max_depth = cfg.max_depth;
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(1)).await;
    settle(&e).await;
    let here = e.play_view(&play).unwrap().beat.unwrap().id;
    // Without it: a few beats ahead, then nothing.
    let near = e.with_graph(|g| g.beats.len());
    e.set_fill(true);
    for _ in 0..10 {
        settle(&e).await;
    }
    e.with_graph(|g| {
        // Everything from here is written and made, down to an ending on every line.
        let mut stack = vec![(here.clone(), 0usize)];
        let mut deepest = 0;
        let mut seen = std::collections::HashSet::new();
        let mut endings = 0;
        let mut forks = 0;
        while let Some((id, d)) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            deepest = deepest.max(d);
            let b = &g.beats[&id];
            if matches!(b.next, Next::Choices { .. }) && d > 0 {
                forks += 1;
            }
            if b.next.exits() == 0 {
                endings += 1;
            }
            assert!(
                yarner_engine::model::screen_ready(g, b),
                "beat {id} has no picture"
            );
            for exit in 0..b.next.exits() {
                let c = g
                    .follow(&id, exit, None)
                    .unwrap_or_else(|| panic!("beat {id} exit {exit} never written"));
                stack.push((c.id.clone(), d + 1));
            }
        }
        assert!(deepest > max_depth, "deepest {deepest}");
        assert!(forks >= 1, "no choice past the first was filled out");
        assert!(endings >= 2, "endings {endings}");
        assert!(
            g.beats.len() > near,
            "{} beats, {near} before",
            g.beats.len()
        );
    });
}

/// A reader at an ending (nothing left below them) doesn't stop keep generating: it goes
/// on with the choices behind them and every other line, until the whole story is made
/// (2026-11-23 alpha: the GPU sat idle 7.5 of 8 hours after a reader stopped at an end).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_generating_goes_on_behind_a_reader_at_an_ending() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.end_at(7);
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        away_secs: 0.0,
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(100)).await;
    settle(&e).await;
    let v = e.play_view(&play).unwrap();
    assert!(
        matches!(v.beat.as_ref().map(|b| &b.next), Some(NextView::End)),
        "the reader never reached an ending"
    );
    let root = e.with_graph(|g| {
        g.beats
            .values()
            .find(|b| b.origin.is_none())
            .map(|b| b.id.clone())
            .expect("an opening")
    });
    let before = e.with_graph(|g| g.beats.len());
    e.set_fill(true);
    for _ in 0..10 {
        settle(&e).await;
    }
    e.with_graph(|g| {
        // Every line from the opening is written and made, down to an ending.
        let mut stack = vec![root.clone()];
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let b = &g.beats[&id];
            assert!(
                yarner_engine::model::screen_ready(g, b),
                "beat {id} has no picture"
            );
            for exit in 0..b.next.exits() {
                let c = g
                    .follow(&id, exit, None)
                    .unwrap_or_else(|| panic!("beat {id} exit {exit} never written"));
                stack.push(c.id.clone());
            }
        }
        assert!(
            g.beats.len() > before,
            "{} beats, {before} before",
            g.beats.len()
        );
    });
}

/// Keep generating goes on from the lines it just wrote, so the backend's kept session
/// state is used: with thinfer's budget (only the most recent few states kept), nearly
/// every beat sends just its new turn. And one state let go restarts only that beat,
/// never the whole session (2026-11-22 alpha: every fill beat re-read 10-12k tokens).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_generating_reuses_the_kept_session_state() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.end_at(10);
    mock.session_node_budget(6);
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        away_secs: 0.0,
        ..Config::default()
    };
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(1)).await;
    settle(&e).await;
    let before = mock.stats();
    e.set_fill(true);
    // Up to a fixed amount of filling, or till the story is filled out (a small one).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let count = || {
        let s = mock.stats();
        s.resumed + s.roots - before.resumed - before.roots
    };
    let (mut last, mut still) = (0, 0);
    while count() < 240 {
        assert!(std::time::Instant::now() < deadline, "fill stalled");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let n = count();
        still = if n == last && e.queue().await.is_empty() {
            still + 1
        } else {
            0
        };
        last = n;
        if still >= 30 {
            break;
        }
    }
    e.set_fill(false);
    let after = mock.stats();
    let resumed = after.resumed - before.resumed;
    let whole = after.roots - before.roots;
    let gone = after.gone - before.gone;
    let jobs = resumed + whole;
    eprintln!("fill: {resumed} of {jobs} resumed, {gone} gone");
    assert_eq!(
        after.sessions_opened, before.sessions_opened,
        "a state let go restarts that beat, not the session"
    );
    assert!(jobs >= 30, "fill wrote {jobs} beats");
    // Each line's first beat may start whole (its state long let go); the rest go on.
    assert!(
        resumed * 100 >= jobs * 78,
        "only {resumed} of {jobs} fill beats resumed ({gone} found their state gone)"
    );
}

/// The LoRA adapters a story switches on for a model go with every picture that model
/// makes, and only those (2026-11-22 alpha: every adapter in the vault went on all of them).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_storys_loras_go_with_its_pictures() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let base = yarner_engine::model::Settings::default();
    let mut loras = std::collections::BTreeMap::new();
    for m in [&base.image_model, &base.image_edit_model] {
        loras.insert(
            m.clone(),
            vec![yarner_engine::model::Lora {
                id: "ink".into(),
                weight: 0.7,
            }],
        );
    }
    e.set_settings(
        yarner_engine::model::Settings {
            loras,
            video: false,
            ..base
        },
        None,
    )
    .unwrap();
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(3)).await;
    settle(&e).await;
    let sent = mock.stats().media_loras;
    assert!(!sent.is_empty());
    assert!(
        sent.iter().all(|l| l == &vec!["ink".to_string()]),
        "{sent:?}"
    );
}

/// "Pause" gives the GPU back: running work is cancelled (and the server going away under
/// it isn't a failure), nothing starts while held, and resuming carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_gives_the_gpu_back_and_resuming_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::realistic(0.05));
    let e = open(&inst, "pw", mock.clone());
    let mut events = e.subscribe();
    setup(&e);
    let play = e.new_play("p").unwrap();
    let _ = e.advance(&play, None).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while e.running() == 0 {
        assert!(std::time::Instant::now() < deadline, "nothing ever ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    e.hold(true);
    // The server stops right away (before every cancel has landed).
    mock.set_down(true);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while e.running() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "work kept running while paused"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(e.running(), 0, "work started while paused");
    assert!(e.idle());
    mock.set_down(false);
    e.hold(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while e.advance(&play, None).unwrap() != Advance::Moved {
        assert!(
            std::time::Instant::now() < deadline,
            "never carried on after resuming"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Ok(ev) = events.try_recv() {
        assert!(
            !matches!(ev, yarner_engine::Event::Error { .. }),
            "a pause showed as an error: {ev:?}"
        );
    }
}

/// Only what a shot shows gets its setup picture as a reference: someone the storyteller
/// lists who isn't in view (the player behind the camera in a first-person shot) isn't
/// sent, or the picture model would paint them in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_whats_in_view_gets_its_reference_picture() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.shot_hides("Mira");
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let face = e
        .upload_media(
            &yarner_testkit::png::placeholder(16, 16, 3),
            "image/png",
            "Mira's face",
            vec!["Mira".into()],
        )
        .unwrap();
    let mira = e.with_graph(|g| {
        g.objects
            .iter()
            .find(|o| o.name == "Mira")
            .map(|o| o.id.clone())
            .unwrap()
    });
    e.put_object(
        Some(&mira),
        "Mira",
        DESCR,
        vec![yarner_engine::model::RefImage {
            use_for: String::new(),
            media: face.clone(),
            caption: "Mira's face".into(),
        }],
        false,
        None,
        None,
    )
    .unwrap();
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(10)).await;
    assert!(!r.gave_up, "{r:?}");
    settle(&e).await;
    e.with_graph(|g| {
        let made: Vec<_> = g
            .media
            .values()
            .filter(|m| m.beat.is_some() && m.detailed)
            .collect();
        assert!(!made.is_empty());
        for m in made {
            assert!(
                !m.objects.iter().any(|o| o == "Mira") && !m.refs.contains(&face),
                "Mira isn't in view but went with the picture: {:?} {:?}",
                m.objects,
                m.refs
            );
        }
    });
}

/// Someone seen only in part (the player's hand at the edge of a first-person shot) gets
/// no reference picture: a face picture for a hand would be painted in as a face.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn someone_seen_only_in_part_gets_no_reference_picture() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.shot_partly("Mira");
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let face = e
        .upload_media(
            &yarner_testkit::png::placeholder(16, 16, 4),
            "image/png",
            "Mira's face",
            vec!["Mira".into()],
        )
        .unwrap();
    let mira = e.with_graph(|g| {
        g.objects
            .iter()
            .find(|o| o.name == "Mira")
            .map(|o| o.id.clone())
            .unwrap()
    });
    e.put_object(
        Some(&mira),
        "Mira",
        DESCR,
        vec![yarner_engine::model::RefImage {
            use_for: String::new(),
            media: face.clone(),
            caption: "Mira's face".into(),
        }],
        false,
        None,
        None,
    )
    .unwrap();
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(10)).await;
    assert!(!r.gave_up, "{r:?}");
    settle(&e).await;
    e.with_graph(|g| {
        let made: Vec<_> = g
            .media
            .values()
            .filter(|m| m.beat.is_some() && m.detailed)
            .collect();
        assert!(!made.is_empty());
        assert!(
            made.iter()
                .all(|m| !m.objects.iter().any(|o| o == "Mira") && !m.refs.contains(&face)),
            "Mira's face went with a shot showing only her hand"
        );
    });
}

/// Every new picture and video is written out in full by the shot step (its own turn,
/// continuing the beat's session), and a thin one is asked again naming its gaps; a video
/// says what moves and what's heard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_pictures_and_videos_are_written_out_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.video_every(2);
    mock.thin_shots(1);
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let play = e.new_play("p").unwrap();
    // Read on until a video has been made (which beats ask for one depends on the order
    // text jobs reach the mock: shot turns are text jobs too).
    let video_made = |e: &Engine| {
        e.with_graph(|g| {
            g.media.values().any(|m| {
                m.kind == yarner_engine::model::MediaKind::Video
                    && m.status == yarner_engine::model::MediaStatus::Ready
                    && m.beat.is_some()
            })
        })
    };
    for _ in 0..6 {
        let r = playtest::run(&e, &play, &fast_player(10)).await;
        assert!(!r.gave_up, "{r:?}");
        settle(&e).await;
        if video_made(&e) {
            break;
        }
    }
    let st = mock.stats();
    e.with_graph(|g| {
        let made: Vec<_> = g
            .media
            .values()
            .filter(|m| m.beat.is_some() && m.status == yarner_engine::model::MediaStatus::Ready)
            // Pictures and videos (voices and sounds have no shot).
            .filter(|m| m.kind != yarner_engine::model::MediaKind::Audio)
            .collect();
        assert!(!made.is_empty());
        let plain: Vec<_> = made
            .iter()
            .filter(|m| !m.detailed)
            .map(|m| (m.kind, m.prompt.chars().take(100).collect::<String>()))
            .collect();
        assert!(plain.is_empty(), "skipped the shot step: {plain:?} {st:?}");
        assert!(
            made.iter()
                .all(|m| m.prompt.split_whitespace().count() >= 40),
            "a thin prompt went out"
        );
        let videos: Vec<_> = made
            .iter()
            .filter(|m| m.kind == yarner_engine::model::MediaKind::Video)
            .collect();
        assert!(!videos.is_empty(), "no video made");
        assert!(
            videos.iter().all(|m| m.prompt.contains("Motion: ")
                && m.prompt.contains("Timeline: ")
                && m.prompt.contains("Sound: ")),
            "a video without motion, a timeline or sound"
        );
        // The thin first reply was asked again: one more shot turn than new media.
        assert!(
            st.shots as usize > made.len(),
            "{} shots, {} made",
            st.shots,
            made.len()
        );
    });
    // Sent as written (the shot already says how they look: no "In it:" added).
    assert!(
        st.media_prompts
            .iter()
            .filter(|p| p.starts_with("Medium wide shot"))
            .all(|p| !p.contains("In it: "))
    );
    // Shot turns continue the beat's node (only the turn is sent).
    assert!(st.resumed > 0);
    // Each video as long as the storyteller asked (3 s here), not always 5.
    assert!(!st.video_seconds.is_empty());
    assert!(
        st.video_seconds.iter().all(|s| *s == 3.0),
        "{:?}",
        st.video_seconds
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_story_can_start_from_another_storys_setup() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{\"title\":\"Lamp\"}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let png = yarner_testkit::png::placeholder(16, 16, 1);
    let m = e
        .upload_media(&png, "image/png", "Tomas", vec!["Tomas".into()])
        .unwrap();
    let spare = e.upload_media(&png, "image/png", "spare", vec![]).unwrap();
    let refs = vec![yarner_engine::model::RefImage {
        use_for: "his face".into(),
        media: m,
        caption: "Tomas".into(),
    }];
    e.put_object(None, "Tomas", "A fisherman.", refs, false, None, None)
        .unwrap();
    let mut s = e.setup_view(None).settings;
    s.picture_max = 3;
    s.own_choices = false;
    e.set_settings(s.clone(), None).unwrap();
    // Play a while, and give a direction from play: it stays with this story.
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(6)).await;
    assert_eq!(r.beats, 6);
    let here = e.play_view(&play).unwrap().beat.unwrap().id;
    e.set_prompt(
        yarner_engine::model::STEER_PROMPT,
        "a storm rolls in",
        Some(here),
        Scope::FromHere,
    )
    .unwrap();
    settle(&e).await;
    let before = e.with_graph(|g| g.beats.len());

    let fresh = inst
        .create_story("pw2-longer", b"{\"title\":\"Lamp again\"}")
        .unwrap();
    e.copy_setup_to(&fresh).unwrap();
    drop(fresh);
    // The old story is untouched.
    assert_eq!(e.with_graph(|g| g.beats.len()), before);

    let e2 = open(&inst, "pw2-longer", mock.clone());
    assert_eq!(e2.with_graph(|g| g.beats.len()), 0, "no beats copied");
    let a = e.setup_view(None);
    let b = e2.setup_view(None);
    let texts = |v: &yarner_engine::view::SetupView| {
        let mut t: Vec<(String, String)> = v
            .prompts
            .iter()
            .map(|p| (p.id.clone(), p.text.clone()))
            .collect();
        t.sort();
        t
    };
    assert_eq!(texts(&a), texts(&b));
    assert!(
        b.prompts
            .iter()
            .all(|p| p.id != yarner_engine::model::STEER_PROMPT)
    );
    assert_eq!(a.settings, b.settings);
    assert_eq!(b.settings.picture_max, 3);
    let names = |v: &yarner_engine::view::SetupView| {
        let mut n: Vec<String> = v.objects.iter().map(|o| o.name.clone()).collect();
        n.sort();
        n
    };
    assert_eq!(names(&a), names(&b));
    let tomas = b.objects.iter().find(|o| o.name == "Tomas").unwrap();
    assert_eq!(tomas.refs.len(), 1);
    assert_eq!(tomas.refs[0].0.use_for, "his face");
    let (bytes, _) = e2.media_bytes(&tomas.refs[0].0.media).unwrap();
    assert_eq!(&bytes[..], &png[..], "the reference picture came along");
    assert!(
        b.library.iter().any(|l| l.id == spare),
        "setup library came along"
    );
    // And it plays from the start.
    let p2 = e2.new_play("p").unwrap();
    let r = playtest::run(&e2, &p2, &fast_player(3)).await;
    assert_eq!(r.beats, 3);
}

/// Does this beat put up a new video (which makes its own sound)?
fn own_video(g: &yarner_engine::model::Graph, b: &yarner_engine::model::Beat) -> bool {
    matches!(&b.show, yarner_engine::model::Show::New { media }
        if g.media[media].kind == yarner_engine::model::MediaKind::Video)
}

/// Mira, talking, with the default deliveries and a description of her voice.
fn talking_mira(e: &Engine) -> String {
    let mira = e
        .setup_view(None)
        .objects
        .iter()
        .find(|o| o.name == "Mira")
        .unwrap()
        .id
        .clone();
    let voice = yarner_engine::view::default_voice("a low, weathered woman's voice, unhurried");
    e.put_object(Some(&mira), "Mira", DESCR, vec![], true, Some(voice), None)
        .unwrap();
    mira
}

fn voice_of(e: &Engine, id: &str) -> yarner_engine::model::Voice {
    e.setup_view(None)
        .objects
        .into_iter()
        .find(|o| o.id == id)
        .unwrap()
        .voice
        .unwrap()
}

/// Make every delivery of an object's voice and wait for them.
async fn make_all(e: &Engine, id: &str) -> yarner_engine::model::Voice {
    let v = voice_of(e, id);
    let all: Vec<usize> = (0..v.deliveries.len()).collect();
    let v = e.make_voice(id, v, &all, None).unwrap();
    settle(e).await;
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_talking_character_speaks_in_the_voice_made_for_it() {
    use yarner_engine::model::{AudioRole, MediaKind, MediaStatus};
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{\"title\":\"Lamp\"}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let mira = talking_mira(&e);
    // Every delivery made at once: one voice, one seed, the same words said each way.
    let v = make_all(&e, &mira).await;
    let made: Vec<_> = mock
        .stats()
        .audio
        .into_iter()
        .filter(|a| a.model == "qwen3-tts-design")
        .collect();
    assert_eq!(made.len(), 6, "{made:?}");
    assert!(
        made.iter()
            .all(|a| a.seed.is_some() && a.seed == made[0].seed)
    );
    assert!(made.iter().all(|a| a.text == v.words && !a.voice));
    assert!(made.iter().any(|a| {
        a.instruct
            .as_deref()
            .is_some_and(|i| i.contains("low, weathered") && i.contains("angry"))
    }));
    let clips: Vec<String> = v
        .deliveries
        .iter()
        .map(|d| d.clip.clone().unwrap())
        .collect();
    e.with_graph(|g| {
        for c in &clips {
            let m = &g.media[c];
            assert_eq!(m.kind, MediaKind::Audio);
            assert_eq!(m.status, MediaStatus::Ready, "{m:?}");
            assert!(m.beat.is_none());
        }
    });
    // One made again alone: a new take, the others kept.
    let again = e
        .make_voice(&mira, voice_of(&e, &mira), &[3], None)
        .unwrap();
    assert_ne!(again.deliveries[3].clip, v.deliveries[3].clip);
    assert_eq!(again.deliveries[0].clip, v.deliveries[0].clip);
    settle(&e).await;
    let v = voice_of(&e, &mira);

    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(12)).await;
    assert!(!r.gave_up, "{r:?}");
    settle(&e).await;
    let lines = mock
        .stats()
        .audio
        .into_iter()
        .filter(|a| a.model == "qwen3-tts")
        .collect::<Vec<_>>();
    assert!(!lines.is_empty(), "no line was spoken");
    // In her clip's voice, with what the clip says.
    assert!(
        lines
            .iter()
            .all(|a| a.voice && a.voice_words.as_deref() == Some(v.words.as_str()))
    );
    let path = e.play_view(&play).unwrap().history;
    e.with_graph(|g| {
        let mut spoken = 0;
        for h in &path {
            let b = &g.beats[&h.beat];
            // A new video says its own lines.
            if own_video(g, b) {
                continue;
            }
            for (i, l) in b.lines.iter().enumerate() {
                let clip = b.voices.get(i).cloned().flatten();
                match l.speaker.as_deref() {
                    // Hers, in her delivery's clip; nobody else talks, narration is silent.
                    Some("Mira") => {
                        let m = &g.media[clip.as_ref().expect("her line is spoken")];
                        let a = m.audio.as_ref().unwrap();
                        assert_eq!(a.role, AudioRole::Line);
                        assert_eq!(a.words, l.text);
                        let d = v.delivery(l.tone.as_deref()).unwrap();
                        assert_eq!(m.refs, vec![d.clip.clone().unwrap()], "{l:?}");
                        // The screen came whole: spoken before it was shown.
                        assert_eq!(m.status, MediaStatus::Ready);
                        spoken += 1;
                    }
                    _ => assert!(clip.is_none(), "{l:?}"),
                }
            }
        }
        assert!(spoken > 0);
    });
    // The storyteller is told her deliveries, and may pick one per line.
    let t = mock.stats();
    assert!(t.audio.iter().any(|a| a.model == "qwen3-tts"));
    // The voice goes along to a new story made from this setup.
    let fresh = inst
        .create_story("pw2-longer", b"{\"title\":\"Lamp again\"}")
        .unwrap();
    e.copy_setup_to(&fresh).unwrap();
    drop(fresh);
    let e2 = open(&inst, "pw2-longer", mock.clone());
    let v2 = voice_of(&e2, &mira);
    assert_eq!(v2.deliveries.len(), 6);
    for d in &v2.deliveries {
        let (bytes, mime) = e2.media_bytes(d.clip.as_ref().unwrap()).unwrap();
        assert!(bytes.starts_with(b"RIFF") && mime == "audio/wav");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_voice_change_leaves_what_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{\"title\":\"Lamp\"}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let mira = talking_mira(&e);
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(4)).await;
    assert!(!r.gave_up);
    settle(&e).await;
    let texts = mock.stats().text_jobs;
    let mut v = voice_of(&e, &mira);
    v.description = "a bright young voice".into();
    e.make_voice(&mira, v, &[0], None).unwrap();
    settle(&e).await;
    assert_eq!(mock.stats().text_jobs, texts, "a voice edit rewrote text");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn narration_is_read_by_the_narrator_and_sounds_play() {
    use yarner_engine::model::{AudioRole, MediaStatus};
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{\"title\":\"Lamp\"}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    // A narrator: only a voice (no clips made: spoken from its description).
    let narrator = e
        .put_object(
            None,
            "Narrator",
            "",
            vec![],
            true,
            Some(yarner_engine::model::Voice {
                description: "a calm, deep storyteller's voice".into(),
                words: String::new(),
                deliveries: vec![],
            }),
            None,
        )
        .unwrap();
    let mut s = e.setup_view(None).settings;
    s.narrator = Some(narrator.clone());
    e.set_settings(s, None).unwrap();
    // The storyteller never hears of the narrator's object.
    e.with_graph(|g| {
        let m = yarner_engine::context::messages(g, &[], None, &Default::default());
        assert!(!m[0].content.contains("Narrator"), "{}", m[0].content);
        assert!(m[0].content.contains("Mira"));
    });
    let play = e.new_play("p").unwrap();
    let r = playtest::run(&e, &play, &fast_player(14)).await;
    assert!(!r.gave_up, "{r:?}");
    settle(&e).await;
    let path = e.play_view(&play).unwrap().history;
    let st = mock.stats();
    e.with_graph(|g| {
        let mut narrated = 0;
        let mut loops = 0;
        for h in &path {
            let b = &g.beats[&h.beat];
            if own_video(g, b) {
                continue;
            }
            for (i, l) in b.lines.iter().enumerate() {
                let clip = b.voices.get(i).cloned().flatten();
                if l.speaker.is_none() {
                    let m = &g.media[clip.as_ref().expect("narration is read")];
                    let a = m.audio.as_ref().unwrap();
                    assert_eq!(a.role, AudioRole::Line);
                    assert!(m.refs.is_empty(), "no clip: from the description");
                    assert!(a.how.contains("calm, deep storyteller"), "{a:?}");
                    narrated += 1;
                } else {
                    // Mira doesn't talk in this story.
                    assert!(clip.is_none());
                }
            }
            for c in &b.sounds {
                let m = &g.media[&c.media];
                assert_eq!(m.status, MediaStatus::Ready);
                assert_eq!(m.audio.as_ref().unwrap().role, AudioRole::Sound);
                assert!((c.line as usize) < b.lines.len());
                loops += usize::from(c.looped);
            }
        }
        assert!(narrated > 0);
        assert!(loops > 0, "the opening starts a background sound");
        // The same sound is made once and played again.
        let doors = g
            .media
            .values()
            .filter(|m| {
                m.audio
                    .as_ref()
                    .is_some_and(|a| a.words.contains("door creaks"))
            })
            .count();
        assert!(doors <= 1, "{doors} door sounds made");
        // The opening's background sound plays on its screen.
        let first = [yarner_engine::model::Hop {
            beat: path[0].beat.clone(),
            exit: None,
        }];
        assert!(yarner_engine::model::ambience_at(g, &first).is_some());
    });
    // Narration went to the design model, one seed for the narrator throughout.
    let narr: Vec<_> = st
        .audio
        .iter()
        .filter(|a| {
            a.instruct
                .as_deref()
                .is_some_and(|i| i.contains("storyteller"))
        })
        .collect();
    assert!(!narr.is_empty());
    assert!(
        narr.iter()
            .all(|a| a.model == "qwen3-tts-design" && a.seed == narr[0].seed)
    );
    let sounds: Vec<_> = st.audio.iter().filter(|a| a.model == "moss-sfx").collect();
    assert!(sounds.iter().any(|a| a.looped && a.seconds == Some(20.0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_video_with_its_own_sound_speaks_for_itself() {
    use yarner_engine::model::{MediaKind, Show};
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{\"title\":\"Lamp\"}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    mock.video_every(1);
    let e = open(&inst, "pw", mock.clone());
    setup(&e);
    let mira = talking_mira(&e);
    let v = make_all(&e, &mira).await;
    // Her picture: a video takes voices only alongside reference pictures.
    let png = yarner_testkit::png::placeholder(16, 16, 1);
    let pic = e
        .upload_media(&png, "image/png", "Mira", vec!["Mira".into()])
        .unwrap();
    let refs = vec![yarner_engine::model::RefImage {
        use_for: String::new(),
        media: pic,
        caption: "Mira".into(),
    }];
    e.put_object(Some(&mira), "Mira", DESCR, refs, true, Some(v), None)
        .unwrap();
    let play = e.new_play("p").unwrap();
    // Videos she speaks in, in view: each gets her voice (and the prompt says whose).
    let hers = |e: &Engine| {
        e.with_graph(|g| {
            g.media
                .values()
                .filter(|m| m.kind == MediaKind::Video)
                .filter(|m| m.status == yarner_engine::model::MediaStatus::Ready)
                .filter(|m| m.objects.iter().any(|o| o == "Mira"))
                .filter(|m| {
                    m.beat.as_ref().is_some_and(|b| {
                        g.beats[b]
                            .lines
                            .iter()
                            .any(|l| l.speaker.as_deref() == Some("Mira"))
                    })
                })
                .count()
        })
    };
    for _ in 0..30 {
        let r = playtest::run(&e, &play, &fast_player(10)).await;
        assert!(!r.gave_up, "{r:?}");
        settle(&e).await;
        if hers(&e) > 0 {
            break;
        }
    }
    let st = mock.stats();
    assert!(st.videos > 0);
    assert!(st.video_ref_audios.iter().all(|n| *n <= 3));
    let voiced = st.video_ref_audios.iter().filter(|n| **n > 0).count();
    let named = st
        .media_prompts
        .iter()
        .filter(|p| p.contains("<Audio 1> is the voice-timbre reference for Mira."))
        .count();
    assert!(hers(&e) > 0, "she never spoke in a video");
    assert_eq!(voiced, hers(&e), "{:?}", st.video_ref_audios);
    assert_eq!(named, voiced);
    e.with_graph(|g| {
        let mut videos = 0;
        for b in g.beats.values() {
            if own_video(g, b) {
                videos += 1;
                // It says its lines and makes its sounds itself.
                assert!(b.voices.is_empty() && b.sounds.is_empty(), "{b:?}");
            }
        }
        assert!(videos > 0, "no video");
    });

    // A video model that makes no sound: its screens are voiced like any other.
    let mut s = e.setup_view(None).settings;
    s.video_model = "fastwan-ti2v-5b".into();
    s.video_ref_model = "fastwan-ti2v-5b".into();
    e.set_settings(s, None).unwrap();
    let before = mock.stats().audio.len();
    let r = playtest::run(&e, &play, &fast_player(10)).await;
    assert!(!r.gave_up, "{r:?}");
    settle(&e).await;
    let path = e.play_view(&play).unwrap().history;
    e.with_graph(|g| {
        for h in &path {
            let b = &g.beats[&h.beat];
            let silent_video = matches!(&b.show, Show::New { media }
                if g.media[media].kind == MediaKind::Video
                    && g.media[media].at > 0
                    && !g.media[media].prompt.contains("voice-timbre"));
            let mira_speaks = b.lines.iter().any(|l| l.speaker.as_deref() == Some("Mira"));
            if silent_video && mira_speaks && !b.voices.is_empty() {
                assert!(b.voices.iter().any(Option::is_some));
            }
        }
    });
    assert!(
        mock.stats().audio.len() > before,
        "nothing voiced after the switch"
    );
}

/// A reader who asked for the next screen before it was ready, then walked off without
/// tapping it once it was: as away as anyone, so "keep generating" goes on (2026-11-23
/// alpha: the queue sat idle for 26 minutes with hours left on it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_generating_goes_on_past_a_ready_screen_left_untapped() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing {
        text: Duration::from_millis(150),
        image: Duration::ZERO,
        video: Duration::ZERO,
        audio: Duration::ZERO,
    });
    mock.end_at(9);
    let story = inst.unlock("pw").unwrap().pop().unwrap();
    let cfg = Config {
        tool_rounds: 0,
        away_secs: 0.0,
        ..Config::default()
    };
    let max_depth = cfg.max_depth;
    let e = Engine::open(story, mock.clone(), cfg).unwrap();
    setup(&e);
    let play = e.new_play("p").unwrap();
    playtest::run(&e, &play, &fast_player(1)).await;
    let here = e.play_view(&play).unwrap().beat.unwrap().id;
    // Asked before the next screen is written: waiting.
    let asked = e.advance(&play, None).unwrap();
    if asked != Advance::Waiting {
        // Written already (a fast machine): nothing to show here.
        return;
    }
    e.set_fill(true);
    let deepest = |e: &Engine| {
        e.with_graph(|g| {
            let mut stack = vec![(here.clone(), 0usize)];
            let mut seen = std::collections::HashSet::new();
            let mut deepest = 0;
            while let Some((id, d)) = stack.pop() {
                if !seen.insert(id.clone()) {
                    continue;
                }
                deepest = deepest.max(d);
                let b = &g.beats[&id];
                for exit in 0..b.next.exits() {
                    if let Some(c) = g.follow(&id, exit, None) {
                        stack.push((c.id.clone(), d + 1));
                    }
                }
            }
            deepest
        })
    };
    // Filling goes past the few screens plain speculation writes.
    let t0 = std::time::Instant::now();
    while deepest(&e) <= max_depth && t0.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Still where they were (never moved on for them).
    assert_eq!(e.play_view(&play).unwrap().beat.unwrap().id, here);
    let deepest = deepest(&e);
    assert!(deepest > max_depth, "deepest {deepest}: filling stopped");
}
