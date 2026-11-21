//! A real session: the full engine on a thinfer-serve, played by a synthetic reader at
//! human pace. Writes the transcript, the pictures and the stall report to `out-dir`.
//!
//! `cargo run --release -p yarner-server --example real_play -- <url|managed:bin> <token-file> <out-dir> [beats] [cps] [sessions|classic] [portrait]`

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use yarner_engine::model::{STORY_PROMPT, STYLE_PROMPT, Scope, Settings};
use yarner_engine::{Config, Engine};
use yarner_format::{Install, KdfParams};
use yarner_server::thinfer::{Options, ThinferBackend};
use yarner_testkit::playtest::{self, Picker, Player};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let token = std::fs::read_to_string(&args[2])
        .unwrap_or_default()
        .trim()
        .to_string();
    let out = std::path::PathBuf::from(&args[3]);
    let beats: usize = args.get(4).and_then(|a| a.parse().ok()).unwrap_or(10);
    let cps: f64 = args.get(5).and_then(|a| a.parse().ok()).unwrap_or(20.0);
    let classic = args.get(6).is_some_and(|a| a == "classic");
    std::fs::create_dir_all(out.join("pictures")).unwrap();
    let data = out.join("data");
    let _ = std::fs::remove_dir_all(&data);

    // `managed:<thinfer-serve path>` starts a private one (and stops it at the end).
    let mut managed = None;
    let backend = if let Some(bin) = args[1].strip_prefix("managed:") {
        let m = yarner_server::managed::Managed::start(yarner_server::managed::Spec {
            bin: bin.into(),
            dir: out.join("thinfer"),
            sources: None,
            download: false,
            keep: false,
            vault_dir: None,
        })
        .await
        .expect("managed thinfer-serve");
        let b = ThinferBackend::new(
            &m.url,
            Options {
                token: Some(m.token.clone()),
                ca_pem: None,
            },
        )
        .unwrap();
        managed = Some(m);
        Arc::new(b)
    } else {
        Arc::new(
            ThinferBackend::new(
                &args[1],
                Options {
                    token: Some(token),
                    ca_pem: None,
                },
            )
            .unwrap(),
        )
    };
    let inst = Install::open_with(&data, KdfParams::insecure_fast()).unwrap();
    let story = inst
        .create_story("probe passphrase", br#"{"title":"The Lamp"}"#)
        .unwrap();
    let cfg = Config {
        read_chars_per_sec: cps,
        sessions: !classic,
        segment: if classic {
            1
        } else {
            Config::default().segment
        },
        ..Config::default()
    };
    let e = Engine::open(story, backend, cfg).unwrap();
    e.set_prompt(
        STORY_PROMPT,
        "Mira keeps a lighthouse on a remote basalt island. Tonight the lamp has started signalling \
         something under the sea, and something is signalling back. Quiet, eerie, warm; a slow-burn \
         mystery with small kindnesses.",
        None,
        Scope::FromHere,
    )
    .unwrap();
    e.set_prompt(
        STYLE_PROMPT,
        "moody watercolor, muted blues and amber lamplight",
        None,
        Scope::FromHere,
    )
    .unwrap();
    e.put_object(
        None,
        "Mira",
        "The keeper. Fifties, weathered hands, green oilskin coat, distrusts the mainland.",
        vec![],
        false,
        None,
        None,
    )
    .unwrap();
    e.put_object(
        None,
        "Tomas",
        "A young fisherman who brings supplies on Thursdays. Talks too much, means well.",
        vec![],
        false,
        None,
        None,
    )
    .unwrap();
    e.put_object(
        None,
        "The Lighthouse",
        "Whitewashed tower on a basalt spit, iron gallery, an old Fresnel lens.",
        vec![],
        false,
        None,
        None,
    )
    .unwrap();
    e.set_settings(
        if args.get(7).is_some_and(|a| a == "portrait") {
            Settings {
                video: false,
                width: 480,
                height: 832,
                orientation: yarner_engine::model::Orientation::Portrait,
                ..Settings::default()
            }
        } else {
            Settings {
                video: false,
                ..Settings::default()
            }
        },
        None,
    )
    .unwrap();

    let play = e.new_play("probe").unwrap();
    let t0 = Instant::now();
    let player = Player {
        chars_per_sec: cps,
        per_line: Duration::from_millis(400),
        picker: Picker::Seeded(5),
        beats,
        max_wait: Duration::from_secs(600),
    };
    let r = playtest::run(&e, &play, &player).await;
    let wall = t0.elapsed().as_secs_f64();

    // Transcript of the path played, with pictures saved alongside.
    let view = e.play_view(&play).unwrap();
    let mut doc = String::new();
    let path = e.plays().into_iter().find(|p| p.id == play).unwrap().path;
    e.with_graph(|g| {
        for (i, h) in path.iter().enumerate() {
            let b = &g.beats[&h.beat];
            if let Some(l) = &b.label {
                let _ = writeln!(doc, "\n## {l}");
            }
            let _ = writeln!(doc, "\n[beat {i}] media: {:?}", b.show);
            for l in &b.lines {
                let _ = writeln!(doc, "  {}: {}", l.speaker.as_deref().unwrap_or("-"), l.text);
            }
            match &b.next {
                yarner_engine::model::Next::Choices { choices } => {
                    for (j, c) in choices.iter().enumerate() {
                        let mark = if h.exit == Some(j) { ">" } else { " " };
                        let _ = writeln!(doc, "  {mark} [{}] {}", j, c.label);
                    }
                }
                yarner_engine::model::Next::End => doc.push_str("  (the end)\n"),
                yarner_engine::model::Next::Rejoin { beat } => {
                    let _ = writeln!(doc, "  (rejoins {beat})");
                }
                _ => {}
            }
        }
    });
    let mut saved = 0;
    let media: Vec<(String, String)> = e.with_graph(|g| {
        g.media
            .values()
            .filter(|m| m.blob.is_some())
            .map(|m| (m.id.clone(), m.caption.clone()))
            .collect()
    });
    for (id, caption) in media {
        if let Ok((bytes, _)) = e.media_bytes(&id) {
            std::fs::write(out.join("pictures").join(format!("{id}.png")), &*bytes).unwrap();
            let _ = writeln!(doc, "picture {id}: {caption}");
            saved += 1;
        }
    }
    let (total, pending) = e.with_graph(|g| {
        (
            g.beats.len(),
            g.beats.values().filter(|b| b.pending).count(),
        )
    });
    let report = format!(
        "wall {wall:.0}s, beats {}, waits {} (total {:.0}s, max {:.0}s), stall {:.0}%, read {:.0}s\n\
         pictures new {} reused {} late {} saved {saved}; beats generated {total} ({pending} pending); \
         last beat {:?}\n",
        r.beats,
        r.waits,
        r.wait_total_ms as f64 / 1000.0,
        r.wait_max_ms as f64 / 1000.0,
        r.stall_ratio() * 100.0,
        r.read_total_ms as f64 / 1000.0,
        r.images_new,
        r.images_reused,
        r.media_late,
        view.beat.map(|b| b.id),
    );
    let mut waits = String::new();
    for w in &r.wait_log {
        let _ = writeln!(
            waits,
            "wait beat {:>2} {:>5.1}s {} running {:?}",
            w.beat,
            w.ms as f64 / 1000.0,
            if w.after_choice {
                "after choice"
            } else {
                "            "
            },
            w.running
        );
    }
    let report = format!("{report}{}{waits}", text_report(&e.text_stats()));
    std::fs::write(out.join("transcript.txt"), format!("{report}\n{doc}")).unwrap();
    let stats = serde_json::to_string_pretty(&e.text_stats()).unwrap();
    std::fs::write(out.join("text_stats.json"), stats).unwrap();
    print!("{report}");
    if let Some(m) = managed {
        m.stop().await;
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Text timing split by whether the call continued a session node.
fn text_report(stats: &[yarner_engine::TextStat]) -> String {
    type F = fn(&yarner_engine::TextStat) -> f64;
    let mut out = String::new();
    for (name, resumed) in [("resumed", true), ("full", false)] {
        let s: Vec<_> = stats
            .iter()
            .filter(|s| s.resumed == resumed && !s.gone)
            .collect();
        if s.is_empty() {
            continue;
        }
        let m = |f: F| median(s.iter().map(|x| f(x)).collect());
        let _ = writeln!(
            out,
            "text {name}: n {} wall {:.1}s first-line {:.1}s prefill {:.1}s/{:.0} tok decode {:.1}s/{:.0} tok (medians)",
            s.len(),
            m(|x| x.wall_secs),
            m(|x| x.first_line_secs.unwrap_or(x.wall_secs)),
            m(|x| x.prefill_secs),
            m(|x| x.prompt_tokens as f64),
            m(|x| x.decode_secs),
            m(|x| x.generated_tokens as f64),
        );
    }
    let gone = stats.iter().filter(|s| s.gone).count();
    let tools = stats.iter().filter(|s| s.tool_round).count();
    let _ = writeln!(
        out,
        "text calls {} (tool rounds {tools}, session gone {gone})",
        stats.len()
    );
    out
}
