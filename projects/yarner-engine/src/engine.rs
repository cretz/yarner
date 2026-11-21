//! The engine: one unlocked story, its generation, and its playthroughs.
//!
//! The scheduler keeps the backend busy generating ahead of every active playthrough.
//! Guardrails that hold whatever the LLM says:
//! - the beat a player is waiting on always comes first,
//! - work for choices not taken is demoted, never discarded (it stays as pending beats),
//! - a choice the player favors is boosted,
//! - beyond the speculation depth nothing new starts.
//!
//! Work that is missing is derived from the graph (exits with no beat, media still
//! requested), so nothing about the queue needs saving: after a restart the scheduler
//! simply finds the same gaps again.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Notify, broadcast};
use yarner_format::Story;
use yarner_format::log::LogFile;

use crate::backend::{
    AudioRequest, Backend, BackendError, ImageRequest, Job, JobId, JobKind, JobOutput, Message,
    ModelCaps, Priority, QueueEntry, SessionRef, TextRequest, VideoRequest,
};
use crate::context::{self, Limits};
use crate::contract::{self, BeatOut, MediaOut, NextOut};
use crate::model::*;
use crate::plays::{Play, PlayRecord, Plays, Save};
use crate::shot;
use crate::view::{self, BeatView, MapView, PlayView};

/// Content-free telemetry (target `yarner_diag`): ids, counts, kinds, timings and error
/// classes only. Never story text, prompts, captions, names or error messages (those can
/// carry the story); the server may write these lines to a file.
macro_rules! diag {
    ($($t:tt)*) => { tracing::info!(target: "yarner_diag", $($t)*) };
}

/// An error's class, for telemetry (its message may quote the story).
fn error_class(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    if m.contains("cancel") {
        "cancelled"
    } else if m.contains("memory") || m.contains("oom") {
        "out_of_memory"
    } else if m.contains("timed out") || m.contains("timeout") {
        "timeout"
    } else if m.contains("connect") || m.contains("unreachable") || m.contains("refused") {
        "unreachable"
    } else if m.contains("json") || m.contains("parse") || m.contains("no lines") {
        "bad_reply"
    } else if m.contains("session") {
        "session"
    } else {
        "other"
    }
}

fn key_kind(k: &TaskKey) -> &'static str {
    match k {
        TaskKey::Gen { .. } => "gen",
        TaskKey::Fork { .. } => "fork",
        TaskKey::Rerun { .. } => "rerun",
        TaskKey::Media { .. } => "media",
        #[allow(unreachable_patterns)]
        _ => "other",
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// How many beats ahead of each player to generate.
    pub speculate_depth: usize,
    /// When everything within `speculate_depth` is written and no media fits, keep writing
    /// further ahead up to this depth before letting long media run anyway.
    pub max_depth: usize,
    /// Jobs of each kind handed to the backend at once.
    pub max_text: usize,
    pub max_image: usize,
    pub max_video: usize,
    pub max_audio: usize,
    /// Jobs of any kind handed to the backend at once. A single-GPU backend runs one at a
    /// time and can't preempt, so anything queued there is a commitment: keep it to what
    /// runs now and decide the rest at the last moment. (A waiting player's beat is
    /// always handed over at once.)
    pub max_jobs: usize,
    /// When a player is waiting, cancel the job running ahead of their beat if it has
    /// more than this many seconds left (its work is redone later). `None` = never.
    pub preempt_secs: Option<f64>,
    /// Seconds the setup must sit unchanged before the opening beat (and its picture) is
    /// written ahead of anyone pressing Start. `None` = only when someone starts.
    pub prefetch_after_secs: Option<f64>,
    /// Tool rounds (search_images, recall) allowed per beat (0 = no tools offered). On
    /// since thinfer 0fa88a5: with tools and a format the model replies in the format or
    /// calls a tool, no free prose (before, every beat ran to max_tokens, 60-80 s).
    pub tool_rounds: usize,
    /// Use backend text sessions (KV cache) so each beat sends only its new turn.
    pub sessions: bool,
    /// Beats per chat segment (1 = every beat gets a fresh full-context prompt).
    pub segment: usize,
    pub retries: usize,
    /// Compact the graph log after this many appended records.
    pub compact_after: usize,
    /// How fast a player reads, for estimating the reading time buffered ahead of them.
    pub read_chars_per_sec: f64,
    pub read_secs_per_line: f64,
    /// A player who hasn't advanced for this long is treated as away: long media (video)
    /// may then run even if it outlasts the text buffer.
    pub away_secs: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            speculate_depth: 2,
            max_depth: 6,
            max_text: 2,
            max_image: 1,
            max_video: 1,
            max_audio: 1,
            max_jobs: 1,
            preempt_secs: Some(1.0),
            // Long enough that an author editing the setup isn't rewritten after every
            // field (2026-11-21 alpha: three 30 s rewrites before Start).
            prefetch_after_secs: Some(30.0),
            tool_rounds: 2,
            sessions: true,
            segment: 24,
            retries: 2,
            compact_after: 5000,
            read_chars_per_sec: 20.0,
            read_secs_per_line: 0.4,
            away_secs: 90.0,
        }
    }
}

pub mod prio {
    use crate::backend::Priority;
    pub const WAITING: Priority = 1000;
    /// A screen right after the one a reader has up: nothing else goes ahead of it, and
    /// text in its way is cancelled (2026-11-21 alpha: readers kept seeing "in line").
    pub const NOW: Priority = 950;
    pub const SETUP_MEDIA: Priority = 900;
    pub const NEXT: Priority = 800;
    pub const MEDIA_NOW: Priority = 700;
    /// Above MEDIA_NOW: the branches of the choice on screen are needed within one
    /// read of the beat; its picture would mostly land after that anyway.
    pub const CHOICE: Priority = 750;
    pub const MEDIA_AHEAD: Priority = 400;
    pub const DEPTH_STEP: Priority = 150;
    pub const FAVOR: Priority = 120;
    pub const VIDEO_PENALTY: Priority = 250;
    pub const BACKLOG: Priority = 100;
    /// The opening, written while the author is still on the setup screen.
    pub const PREFETCH: Priority = 300;
    pub const ORPHAN: Priority = -100;

    /// The rung of the ladder a priority is on: work on a higher rung interrupts work on
    /// a lower one (a picture or video pauses and resumes; text is cancelled and written
    /// again), work on the same rung takes turns. Top: what a reader waits on or picked;
    /// then their next screens (with the pictures they need); then on down, a rung per
    /// hundred (2026-11-22, the user: "anything the user is waiting on is highest
    /// priority and interrupts other things, and the next thing for a user is next
    /// highest priority and can interrupt others, and so on").
    pub fn tier(p: Priority) -> Priority {
        if p >= WAITING - 1 {
            100
        } else if p >= NOW - 20 {
            99
        } else {
            p.div_euclid(100).min(98)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A beat was created or replaced.
    Beat {
        beat: BeatId,
    },
    Media {
        media: MediaId,
    },
    /// A playthrough moved or changed.
    Play {
        play: String,
    },
    /// Setup (prompts, objects, settings) changed.
    Setup,
    /// A beat being written has more lines (`from` is "" for the opening beat).
    Draft {
        from: BeatId,
        exit: usize,
    },
    /// A generation failed.
    Error {
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("storage: {0}")]
    Format(#[from] yarner_format::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TaskKey {
    /// Generate the beat behind an exit (or the opening beat when `from` is empty).
    Gen {
        from: BeatId,
        epoch: u32,
        exit: usize,
    },
    /// A new version of the beat behind an exit.
    Fork {
        from: BeatId,
        epoch: u32,
        exit: usize,
    },
    /// Regenerate a beat in place.
    Rerun {
        beat: BeatId,
    },
    Media {
        media: MediaId,
    },
}

struct Task {
    kind: JobKind,
    priority: Priority,
    job: Option<JobId>,
    started: std::time::Instant,
    /// Cancelled to make way for a waiting player.
    preempted: bool,
    /// Sent to cut in ahead of what's running.
    cut_in: bool,
    /// A worker has taken one of its jobs (it stays "being made" across tool rounds).
    began: bool,
    /// The priority the backend has for its job: `priority` once a change has reached
    /// it. What cuts in over what is judged by this, the backend's view (a job just
    /// demoted, the change still on its way, would be paused at its old priority).
    sent: Priority,
}

struct Active {
    favor: Option<usize>,
    /// The exit the player tried to take and is waiting on.
    waiting: Option<usize>,
    /// The beat the player just arrived at for the first time (still unseen this visit).
    fresh: Option<BeatId>,
    last_move: std::time::Instant,
    /// Since when the player has been waiting on the next screen.
    waiting_since: Option<std::time::Instant>,
    /// When the wait was last written to the diagnostics (every 10 s while it lasts).
    wait_logged: Option<std::time::Instant>,
}

impl Default for Active {
    fn default() -> Self {
        Self {
            favor: None,
            waiting: None,
            fresh: None,
            last_move: std::time::Instant::now(),
            waiting_since: None,
            wait_logged: None,
        }
    }
}

struct State {
    graph: Graph,
    log: LogFile,
    appended: usize,
    plays_log: LogFile,
    plays: Plays,
    active: HashMap<String, Active>,
    tasks: HashMap<TaskKey, Task>,
    /// Gen/fork requests waiting for a slot: key -> path to generate along.
    wanted_paths: HashMap<TaskKey, Path>,
    /// A direction for one requested generation (a fork of the opening: no beat before
    /// it to anchor a prompt at).
    notes: HashMap<TaskKey, String>,
    depth: usize,
    /// "Keep generating while I'm away": once a reader is away and nothing near them is
    /// left, the story is filled out wide and deep, likeliest first, until the time is up.
    fill: bool,
    paused: bool,
    /// The GPU is given back ("Pause"): nothing runs, and what was running was
    /// cancelled, to be made again on resume (never counted as a failure).
    held: bool,
    /// Learned job durations (seconds), for the media gate.
    durations: HashMap<JobKind, f64>,
    /// The backend text session (model, id) and each beat's node in it.
    session: Option<(String, String)>,
    nodes: HashMap<BeatId, String>,
    /// The beats whose nodes were made or used last, newest last: the backend keeps its
    /// most recently used session state, so going on from these resumes.
    warm: VecDeque<BeatId>,
    /// Lines of beats still being written, by the exit they're for ("" = the opening).
    drafts: HashMap<(BeatId, usize), Vec<Line>>,
    /// Recent text generations, newest last (bounded), for tuning and the admin view.
    text_stats: std::collections::VecDeque<TextStat>,
    /// When the setup (prompts, objects, settings) last changed.
    setup_at: std::time::Instant,
    /// Work that failed: how many times in a row, and not to retry before when.
    failures: HashMap<TaskKey, (u32, std::time::Instant)>,
    /// What the backend's models can do (fetched once it answers).
    models: Vec<crate::backend::ModelCaps>,
    /// Characters of each beat reply streamed so far, by the exit it's for.
    written: HashMap<(BeatId, usize), usize>,
    /// How long a beat reply usually is (characters, a running average): with
    /// `written`, how far a beat being written is.
    reply_chars: f64,
    /// Wanted but not sent yet (waiting for a free slot), most urgent first, from the
    /// last tick: shown in the queue as lined up.
    lined: Vec<(TaskKey, JobKind, Priority)>,
}

/// Can jobs of this kind pause (with the story's model for it)? Then a waiting reader's
/// text goes first by pausing them, instead of cancelling (losing work) or waiting.
fn pausable(st: &State, kind: JobKind) -> bool {
    let s = Resolver::new(&st.graph, &[], None).settings();
    let ids: [&str; 2] = match kind {
        JobKind::Image => [&s.image_model, &s.image_edit_model],
        JobKind::Video => [&s.video_model, &s.video_ref_model],
        // Speech is a few seconds a line, and nothing waits long behind it.
        JobKind::Text | JobKind::Audio => return false,
    };
    // The story's models the backend has must all pause (one it doesn't have can't run).
    let found: Vec<&crate::backend::ModelCaps> = ids
        .iter()
        .filter_map(|id| st.models.iter().find(|m| m.id == *id))
        .collect();
    !found.is_empty() && found.iter().all(|m| m.pausable)
}

/// Tries a picture or video gets before it's given up on (failed: the screen keeps the
/// picture before it).
const MEDIA_ATTEMPTS: u32 = 3;

/// Wait before retrying work that failed `n` times in a row: 2 s, 4 s, ... up to a minute
/// (a backend that's down isn't hammered, one that recovers is used again soon).
fn backoff(n: u32) -> Duration {
    Duration::from_secs((1u64 << n.min(6)).min(60))
}

/// Medians over recent beat replies.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct TextSummary {
    pub samples: usize,
    pub wall_secs: f64,
    pub first_line_secs: f64,
    pub prefill_secs: f64,
    /// Share that continued a cached session node.
    pub resumed: f64,
}

/// How one text generation (one backend call) went.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct TextStat {
    /// Seconds from submit to the reply.
    pub wall_secs: f64,
    /// Seconds from submit to the first whole line streamed (None: none streamed).
    pub first_line_secs: Option<f64>,
    pub prefill_secs: f64,
    pub decode_secs: f64,
    pub prompt_tokens: u32,
    pub generated_tokens: u32,
    /// Continued a session node (only the new turn was sent).
    pub resumed: bool,
    /// Messages sent.
    pub messages: usize,
    /// Scheduler priority it ran at.
    pub priority: Priority,
    /// The reply was a tool call round, not the beat.
    pub tool_round: bool,
    /// The cached session was gone and this call failed for it.
    pub gone: bool,
}

struct Inner {
    story: Story,
    backend: Arc<dyn Backend>,
    cfg: Config,
    st: Mutex<State>,
    events: broadcast::Sender<Event>,
    wake: Arc<Notify>,
    /// Changes that read a beat and write it back hold this, so none undoes another.
    edits: Mutex<()>,
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

/// What `advance` did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Advance {
    Moved,
    /// The next beat isn't ready yet; it's now top priority.
    Waiting,
    Ended,
}

fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Option<T> {
    serde_json::from_slice(bytes).ok()
}

impl Engine {
    pub fn open(story: Story, backend: Arc<dyn Backend>, cfg: Config) -> Result<Self> {
        let (log, records) = story.open_graph()?;
        let mut graph = Graph::default();
        let n = records.len();
        for r in records {
            if let Some(rec) = decode::<Record>(&r) {
                graph.apply(rec);
            }
        }
        let (plays_log, precs) = story.open_plays()?;
        let mut plays = Plays::default();
        for r in precs {
            if let Some(rec) = decode::<PlayRecord>(&r) {
                plays.apply(rec);
            }
        }
        let (events, _) = broadcast::channel(256);
        let wake = Arc::new(Notify::new());
        let depth = cfg.speculate_depth;
        let inner = Arc::new(Inner {
            edits: Mutex::new(()),
            story,
            backend,
            cfg,
            st: Mutex::new(State {
                graph,
                log,
                appended: n,
                plays_log,
                plays,
                active: HashMap::new(),
                tasks: HashMap::new(),
                wanted_paths: HashMap::new(),
                notes: HashMap::new(),
                depth,
                fill: false,
                held: false,
                paused: false,
                durations: HashMap::new(),
                session: None,
                nodes: HashMap::new(),
                warm: VecDeque::new(),
                drafts: HashMap::new(),
                text_stats: Default::default(),
                setup_at: std::time::Instant::now(),
                failures: HashMap::new(),
                models: vec![],
                written: HashMap::new(),
                reply_chars: 900.0,
                lined: Vec::new(),
            }),
            events,
            wake: wake.clone(),
        });
        tokio::spawn(scheduler(Arc::downgrade(&inner), wake));
        Ok(Self { inner })
    }

    pub fn story(&self) -> &Story {
        &self.inner.story
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.st.lock().expect("engine state")
    }

    fn emit(&self, e: Event) {
        let _ = self.inner.events.send(e);
    }

    fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Recent text generations, oldest first.
    pub fn text_stats(&self) -> Vec<TextStat> {
        self.st().text_stats.iter().cloned().collect()
    }

    /// How writing has gone lately (the last `n` beat replies), for the admin view.
    pub fn text_summary(&self, n: usize) -> Option<TextSummary> {
        let st = self.st();
        let recent: Vec<&TextStat> = st
            .text_stats
            .iter()
            .rev()
            .filter(|s| !s.tool_round && !s.gone)
            .take(n)
            .collect();
        if recent.is_empty() {
            return None;
        }
        let median = |f: &dyn Fn(&TextStat) -> f64| {
            let mut v: Vec<f64> = recent.iter().map(|s| f(s)).collect();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        Some(TextSummary {
            samples: recent.len(),
            wall_secs: median(&|s| s.wall_secs),
            first_line_secs: median(&|s| s.first_line_secs.unwrap_or(s.wall_secs)),
            prefill_secs: median(&|s| s.prefill_secs),
            resumed: recent.iter().filter(|s| s.resumed).count() as f64 / recent.len() as f64,
        })
    }

    fn record_text(&self, s: TextStat) {
        diag!(
            event = "text",
            wall_s = s.wall_secs,
            first_line_s = s.first_line_secs.unwrap_or(-1.0),
            prefill_s = s.prefill_secs,
            decode_s = s.decode_secs,
            prompt_tok = s.prompt_tokens,
            gen_tok = s.generated_tokens,
            resumed = s.resumed,
            priority = s.priority,
            tool_round = s.tool_round,
            gone = s.gone,
        );
        let mut st = self.st();
        if st.text_stats.len() >= 512 {
            st.text_stats.pop_front();
        }
        st.text_stats.push_back(s);
    }

    /// Read the graph (for views and tests).
    pub fn with_graph<R>(&self, f: impl FnOnce(&Graph) -> R) -> R {
        f(&self.st().graph)
    }

    /// Speculation depth (raise it for "keep generating").
    pub fn set_depth(&self, depth: usize) {
        self.st().depth = depth;
        self.wake();
    }

    /// Fill the story out while readers are away (wide and deep, likeliest first, with
    /// no depth limit) once nothing near them is left to make.
    pub fn set_fill(&self, on: bool) {
        self.st().fill = on;
        self.wake();
    }

    /// Stop starting new work (running jobs finish).
    pub fn set_paused(&self, paused: bool) {
        self.st().paused = paused;
        self.wake();
    }

    /// Give the GPU back ("Pause") or take it again: while held nothing is started and
    /// what's running is cancelled now (cancelled work is wanted again, so it's made
    /// once resumed; nothing counts as failed).
    pub fn hold(&self, on: bool) {
        let jobs: Vec<JobId> = {
            let mut st = self.st();
            st.held = on;
            if !on {
                vec![]
            } else {
                st.tasks
                    .values_mut()
                    .filter_map(|t| {
                        t.preempted = true;
                        t.job.clone()
                    })
                    .collect()
            }
        };
        if !jobs.is_empty() {
            let e = self.clone();
            tokio::spawn(async move {
                for j in jobs {
                    e.inner.backend.cancel(&j).await;
                }
            });
        }
        self.wake();
    }

    /// Work still on the backend (a hold waits for it to be gone).
    pub fn running(&self) -> usize {
        self.st().tasks.len()
    }

    /// Stop for good (story locked): start nothing new and cancel queued backend jobs, so
    /// tasks holding the story finish and its key can be dropped.
    pub async fn shutdown(&self) {
        let jobs: Vec<JobId> = {
            let mut st = self.st();
            st.paused = true;
            st.wanted_paths.clear();
            st.active.clear();
            st.tasks.values().filter_map(|t| t.job.clone()).collect()
        };
        for j in jobs {
            self.inner.backend.cancel(&j).await;
        }
        let session = self.st().session.take();
        if let Some((_, s)) = session {
            self.inner.backend.drop_session(&s).await;
        }
        self.wake();
    }

    /// Nothing running and nothing the scheduler would start now.
    pub fn idle(&self) -> bool {
        let st = self.st();
        st.tasks.is_empty()
            && st.wanted_paths.is_empty()
            && (st.paused || st.held || schedule(&st, &self.inner.cfg).is_empty())
    }

    pub async fn queue(&self) -> Vec<QueueEntry> {
        self.queue_for(None).await
    }

    /// The work under way and lined up, each said in the terms of the reader of `play`
    /// ("Picture for the next screen"): sent jobs first, then what waits for a slot.
    pub async fn queue_for(&self, play: Option<&str>) -> Vec<QueueEntry> {
        let mut q = self.inner.backend.queue().await;
        let st = self.st();
        let g = &st.graph;
        let reader = play
            .and_then(|p| st.plays.plays.get(p))
            .and_then(|p| p.path.last())
            .map(|h| h.beat.clone());
        for e in &mut q {
            let Some((k, t)) = st.tasks.iter().find(|(_, t)| t.job.as_ref() == Some(&e.id)) else {
                continue;
            };
            e.what = Some(what(g, k));
            e.label = Some(label_for(g, k, reader.as_ref()));
            e.secs = Some(t.started.elapsed().as_secs_f64());
        }
        q.sort_by_key(|e| (!e.running, std::cmp::Reverse(e.priority)));
        for (k, kind, p) in &st.lined {
            q.push(QueueEntry {
                id: JobId::default(),
                kind: Some(*kind),
                priority: *p,
                running: false,
                progress: None,
                what: Some(what(g, k)),
                label: Some(label_for(g, k, reader.as_ref())),
                secs: None,
                lined: true,
            });
        }
        q
    }

    // Setup.

    pub fn set_prompt(&self, id: &str, text: &str, anchor: Anchor, scope: Scope) -> Result<RevId> {
        diag!(
            event = "setup",
            what = "prompt",
            everywhere = anchor.is_none()
        );
        let rev = new_id("r");
        self.write(Record::Prompt(PromptRev {
            id: id.into(),
            rev: rev.clone(),
            anchor,
            scope,
            text: text.into(),
            at: now_ms(),
        }))?;
        self.emit(Event::Setup);
        self.wake();
        Ok(rev)
    }

    /// Make a revision apply everywhere: a root copy, every other revision of it retired.
    pub fn make_everywhere(&self, rev: &str) -> Result<RevId> {
        let (rec, retire) = {
            let st = self.st();
            if let Some(p) = st.graph.prompts.iter().find(|p| p.rev == rev) {
                let others = st
                    .graph
                    .prompts
                    .iter()
                    .filter(|o| o.id == p.id)
                    .map(|o| o.rev.clone());
                let mut np = p.clone();
                np.rev = new_id("r");
                np.anchor = None;
                np.scope = Scope::FromHere;
                np.at = now_ms();
                (Record::Prompt(np), others.collect::<Vec<_>>())
            } else if let Some(o) = st.graph.objects.iter().find(|o| o.rev == rev) {
                let others = st
                    .graph
                    .objects
                    .iter()
                    .filter(|x| x.id == o.id)
                    .map(|x| x.rev.clone());
                let mut no = o.clone();
                no.rev = new_id("r");
                no.anchor = None;
                no.at = now_ms();
                (Record::Object(no), others.collect::<Vec<_>>())
            } else {
                return Err(EngineError::NotFound(rev.into()));
            }
        };
        let new_rev = match &rec {
            Record::Prompt(p) => p.rev.clone(),
            Record::Object(o) => o.rev.clone(),
            _ => unreachable!(),
        };
        self.write(rec)?;
        self.write(Record::Retire { revs: retire })?;
        self.emit(Event::Setup);
        Ok(new_rev)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn put_object(
        &self,
        id: Option<&str>,
        name: &str,
        description: &str,
        refs: Vec<RefImage>,
        talks: bool,
        voice: Option<Voice>,
        anchor: Anchor,
    ) -> Result<String> {
        let id = id.map(str::to_string).unwrap_or_else(|| new_id("o"));
        diag!(
            event = "setup",
            what = "object",
            everywhere = anchor.is_none()
        );
        self.write(Record::Object(ObjectRev {
            id: id.clone(),
            rev: new_id("r"),
            anchor,
            name: name.into(),
            description: description.into(),
            refs,
            talks,
            voice,
            removed: false,
            at: now_ms(),
        }))?;
        self.emit(Event::Setup);
        Ok(id)
    }

    /// Make clips of an object's voice (it's saved with `voice` as given): each delivery
    /// in `which`, from the voice's description and words. The ones made together share
    /// one new seed, so they're one voice told to say the same words in different ways;
    /// one made again alone gets a seed of its own (a new take).
    pub fn make_voice(
        &self,
        id: &str,
        mut voice: Voice,
        which: &[usize],
        anchor: Anchor,
    ) -> Result<Voice> {
        let last = self
            .st()
            .graph
            .objects
            .iter()
            .rfind(|o| o.id == id)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(id.into()))?;
        if voice.description.trim().is_empty() {
            return Err(EngineError::Invalid(
                "describe the voice first: how it sounds".into(),
            ));
        }
        let seed = u64::from_le_bytes(yarner_format::crypto::random_array());
        let words = match voice.words.trim() {
            "" => DEFAULT_VOICE_WORDS.to_string(),
            w => w.to_string(),
        };
        voice.words = words.clone();
        let now = now_ms();
        let mut media = Vec::new();
        for &i in which {
            let description = voice.description.clone();
            let Some(d) = voice.deliveries.get_mut(i) else {
                continue;
            };
            let mid = new_id("m");
            media.push(Media {
                id: mid.clone(),
                kind: MediaKind::Audio,
                status: MediaStatus::Requested,
                blob: None,
                mime: None,
                caption: d.name.clone(),
                objects: vec![id.to_string()],
                prompt: String::new(),
                refs: vec![],
                width: 0,
                height: 0,
                beat: None,
                error: None,
                at: now,
                detailed: false,
                seconds: None,
                audio: Some(AudioMeta {
                    role: AudioRole::Voice,
                    words: words.clone(),
                    how: voice_instruct(&description, &d.how),
                    ref_words: String::new(),
                    looped: false,
                    seed: Some(seed),
                }),
            });
            d.clip = Some(mid);
            d.words = words.clone();
        }
        diag!(event = "setup", what = "voice", clips = media.len());
        for m in media {
            self.write(Record::Media(m))?;
        }
        self.write(Record::Object(ObjectRev {
            rev: new_id("r"),
            anchor,
            talks: true,
            voice: Some(voice.clone()),
            at: now_ms(),
            ..last
        }))?;
        self.emit(Event::Setup);
        self.wake();
        Ok(voice)
    }

    pub fn remove_object(&self, id: &str, anchor: Anchor) -> Result<()> {
        let last = self
            .st()
            .graph
            .objects
            .iter()
            .rfind(|o| o.id == id)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(id.into()))?;
        self.write(Record::Object(ObjectRev {
            rev: new_id("r"),
            anchor,
            removed: true,
            at: now_ms(),
            ..last
        }))?;
        self.emit(Event::Setup);
        Ok(())
    }

    pub fn set_settings(&self, settings: Settings, anchor: Anchor) -> Result<()> {
        diag!(
            event = "setup",
            what = "settings",
            everywhere = anchor.is_none()
        );
        let sized = settings.clone();
        let size_for = |k| sized.size_for(k);
        let everywhere = anchor.is_none();
        self.write(Record::Settings(SettingsRev {
            rev: new_id("r"),
            anchor,
            settings,
            at: now_ms(),
        }))?;
        // A new shape or size: pictures written ahead that nobody has seen yet are made
        // again at it (the words stay; see `model::is_stale`).
        if everywhere {
            let redo: Vec<Media> = {
                let st = self.st();
                let g = &st.graph;
                g.media
                    .values()
                    .filter(|m| {
                        m.kind != MediaKind::Audio
                            && (m.width, m.height) != size_for(m.kind)
                            && m.status != MediaStatus::Failed
                    })
                    .filter(|m| {
                        m.beat
                            .as_ref()
                            .and_then(|b| g.beats.get(b))
                            .is_some_and(|b| b.pending)
                    })
                    .map(|m| Media {
                        status: MediaStatus::Requested,
                        blob: None,
                        mime: None,
                        width: size_for(m.kind).0,
                        height: size_for(m.kind).1,
                        ..m.clone()
                    })
                    .collect()
            };
            for m in redo {
                self.write(Record::Media(m))?;
            }
        }
        self.emit(Event::Setup);
        self.wake();
        Ok(())
    }

    /// Store an uploaded image as catalog media.
    pub fn upload_media(
        &self,
        bytes: &[u8],
        mime: &str,
        caption: &str,
        objects: Vec<String>,
    ) -> Result<MediaId> {
        let blob = self.inner.story.put_blob(bytes)?;
        let id = new_id("m");
        let kind = if mime.starts_with("video/") {
            MediaKind::Video
        } else if mime.starts_with("audio/") {
            MediaKind::Audio
        } else {
            MediaKind::Image
        };
        self.write(Record::Media(Media {
            id: id.clone(),
            kind,
            status: MediaStatus::Ready,
            blob: Some(blob),
            mime: Some(mime.into()),
            caption: caption.into(),
            objects,
            prompt: String::new(),
            refs: vec![],
            width: 0,
            height: 0,
            beat: None,
            error: None,
            at: now_ms(),
            detailed: false,
            seconds: None,
            audio: None,
        }))?;
        self.emit(Event::Media { media: id.clone() });
        Ok(id)
    }

    /// Ask for a new image outside the story (designing reference pictures).
    pub fn request_image(
        &self,
        prompt: &str,
        caption: &str,
        objects: Vec<String>,
        refs: Vec<MediaId>,
    ) -> Result<MediaId> {
        let s = Resolver::new(&self.st().graph, &[], None).settings();
        let id = new_id("m");
        self.write(Record::Media(Media {
            id: id.clone(),
            kind: MediaKind::Image,
            status: MediaStatus::Requested,
            blob: None,
            mime: None,
            caption: caption.into(),
            objects,
            prompt: prompt.into(),
            refs,
            width: s.width,
            height: s.height,
            beat: None,
            error: None,
            at: now_ms(),
            detailed: false,
            seconds: None,
            audio: None,
        }))?;
        self.wake();
        Ok(id)
    }

    /// Decrypted media bytes and MIME type.
    pub fn media_bytes(&self, id: &str) -> Result<(zeroize::Zeroizing<Vec<u8>>, String)> {
        let (blob, mime) = {
            let st = self.st();
            let m = st
                .graph
                .media
                .get(id)
                .ok_or_else(|| EngineError::NotFound(id.into()))?;
            (
                m.blob
                    .clone()
                    .ok_or_else(|| EngineError::NotFound(format!("{id} not ready")))?,
                m.mime
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into()),
            )
        };
        Ok((self.inner.story.read_blob(&blob)?, mime))
    }

    /// Random access to media (for ranged responses).
    pub fn media_reader(
        &self,
        id: &str,
    ) -> Result<(yarner_format::blob::BlobReader<std::fs::File>, String)> {
        let (blob, mime) = {
            let st = self.st();
            let m = st
                .graph
                .media
                .get(id)
                .ok_or_else(|| EngineError::NotFound(id.into()))?;
            (
                m.blob
                    .clone()
                    .ok_or_else(|| EngineError::NotFound(format!("{id} not ready")))?,
                m.mime
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into()),
            )
        };
        Ok((self.inner.story.open_blob(&blob)?, mime))
    }

    // Plays.

    pub fn plays(&self) -> Vec<Play> {
        let mut v: Vec<Play> = self.st().plays.plays.values().cloned().collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.at));
        v
    }

    pub fn new_play(&self, name: &str) -> Result<String> {
        let play = Play {
            id: new_id("p"),
            name: name.into(),
            path: vec![],
            at: now_ms(),
            deleted: false,
        };
        let id = play.id.clone();
        self.write_play(PlayRecord::Play(play))?;
        Ok(id)
    }

    pub fn delete_play(&self, id: &str) -> Result<()> {
        let mut p = self.play(id)?;
        p.deleted = true;
        self.st().active.remove(id);
        self.write_play(PlayRecord::Play(p))
    }

    fn play(&self, id: &str) -> Result<Play> {
        self.st()
            .plays
            .plays
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(id.into()))
    }

    /// Mark a playthrough as being played: the scheduler works ahead of it.
    pub fn activate(&self, play: &str) {
        self.st().active.entry(play.into()).or_default();
        self.wake();
    }

    pub fn deactivate(&self, play: &str) {
        self.st().active.remove(play);
        self.wake();
    }

    /// Boost one choice of the current beat.
    pub fn favor(&self, play: &str, exit: Option<usize>) {
        self.st().active.entry(play.into()).or_default().favor = exit;
        self.wake();
    }

    fn set_path(&self, play: &str, path: Path) -> Result<()> {
        if let Some(a) = self.st().active.get_mut(play)
            && a.fresh.as_ref() != path.last().map(|h| &h.beat)
        {
            a.fresh = None;
        }
        let mut p = self.play(play)?;
        p.path = path;
        p.at = now_ms();
        self.write_play(PlayRecord::Play(p))?;
        self.emit(Event::Play { play: play.into() });
        Ok(())
    }

    /// The player asked for the next screen and it isn't ready: put it first.
    fn mark_waiting(&self, play: &str, exit: usize) {
        let mut st = self.st();
        let a = st.active.entry(play.into()).or_default();
        a.waiting = Some(exit);
        a.waiting_since.get_or_insert_with(std::time::Instant::now);
    }

    /// Move a playthrough forward through `exit` (default: the only one).
    pub fn advance(&self, play: &str, exit: Option<usize>) -> Result<Advance> {
        self.activate(play);
        let p = self.play(play)?;
        let (path, taken) = {
            let st = self.st();
            let g = &st.graph;
            if p.path.is_empty() {
                let Some(root) = g.root.clone() else {
                    drop(st);
                    self.mark_waiting(play, 0);
                    self.wake();
                    return Ok(Advance::Waiting);
                };
                // Written for an older setup: wait for the rewrite (unless it was written
                // since this reader asked: see `opening_theirs`).
                if opening_stale(g) && !opening_theirs(&st, play) {
                    drop(st);
                    self.mark_waiting(play, 0);
                    self.wake();
                    return Ok(Advance::Waiting);
                }
                if !screen_ready(g, &g.beats[&root]) {
                    // Shown only once its picture is painted.
                    drop(st);
                    self.mark_waiting(play, 0);
                    self.wake();
                    return Ok(Advance::Waiting);
                }
                let pending = g.beats[&root].pending;
                (
                    vec![Hop {
                        beat: root.clone(),
                        exit: None,
                    }],
                    pending.then_some(root),
                )
            } else {
                let last = p.path.last().expect("non-empty");
                let beat = g
                    .beats
                    .get(&last.beat)
                    .ok_or_else(|| EngineError::NotFound(last.beat.clone()))?;
                let exits = beat.next.exits();
                if exits == 0 {
                    return Ok(Advance::Ended);
                }
                let exit = exit.unwrap_or(0);
                if exit >= exits {
                    return Err(EngineError::Invalid(format!("no exit {exit}")));
                }
                let unseen_stale = |c: &Beat| {
                    let mut with = p.path.clone();
                    with.last_mut().expect("non-empty").exit = Some(exit);
                    with.push(Hop {
                        beat: c.id.clone(),
                        exit: None,
                    });
                    c.pending && is_stale(g, &with)
                };
                let Some(child) = g
                    .follow(&beat.id, exit, None)
                    .filter(|c| screen_ready(g, c) && !unseen_stale(c))
                else {
                    // Not written yet, or its picture isn't painted: the screen comes
                    // whole (text and picture together), never over the wrong picture.
                    drop(st);
                    self.mark_waiting(play, exit);
                    self.wake();
                    return Ok(Advance::Waiting);
                };
                let mut path = p.path.clone();
                path.last_mut().expect("non-empty").exit = Some(exit);
                path.push(Hop {
                    beat: child.id.clone(),
                    exit: None,
                });
                (path, child.pending.then(|| child.id.clone()))
            }
        };
        if let Some(b) = taken {
            self.write(Record::Taken { beat: b })?;
        }
        let beat = path.last().expect("non-empty").beat.clone();
        let fresh = !self.st().plays.seen.contains(&beat);
        if fresh {
            self.write_play(PlayRecord::Seen {
                beats: vec![beat.clone()],
            })?;
        }
        {
            let mut st = self.st();
            let a = st.active.entry(play.into()).or_default();
            if let Some(t) = a.waiting_since.take() {
                diag!(
                    event = "waited",
                    secs = t.elapsed().as_secs_f64(),
                    depth = path.len()
                );
            }
            a.favor = None;
            a.waiting = None;
            a.fresh = fresh.then(|| beat.clone());
            a.last_move = std::time::Instant::now();
        }
        self.set_path(play, path)?;
        self.wake();
        Ok(Advance::Moved)
    }

    /// Step back one beat.
    pub fn back(&self, play: &str) -> Result<()> {
        let mut path = self.play(play)?.path;
        if path.len() <= 1 {
            return Ok(());
        }
        path.pop();
        path.last_mut().expect("non-empty").exit = None;
        self.set_path(play, path)
    }

    /// Go back to step `index` of the current path (rollback / map in play mode).
    pub fn rewind_to(&self, play: &str, index: usize) -> Result<()> {
        let mut path = self.play(play)?.path;
        if index >= path.len() {
            return Err(EngineError::Invalid("past the end of the path".into()));
        }
        path.truncate(index + 1);
        path.last_mut().expect("non-empty").exit = None;
        self.set_path(play, path)
    }

    /// Edit mode: continue this playthrough at `beat` as if a path to it had been taken.
    /// Uses `via` if given and valid, else the current path's prefix, else any path.
    pub fn jump(&self, play: &str, beat: &str, via: Option<Path>) -> Result<()> {
        let cur = self.play(play)?.path;
        let path = {
            let st = self.st();
            let g = &st.graph;
            if let Some(v) =
                via.filter(|v| validate_path(g, v) && v.last().is_some_and(|h| h.beat == beat))
            {
                v
            } else if let Some(i) = cur.iter().position(|h| h.beat == beat) {
                let mut p = cur[..=i].to_vec();
                p.last_mut().expect("non-empty").exit = None;
                p
            } else {
                find_path(g, beat)
                    .ok_or_else(|| EngineError::NotFound(format!("no path to {beat}")))?
            }
        };
        self.set_path(play, path)?;
        self.wake();
        Ok(())
    }

    /// Edit mode: regenerate the current beat in place, optionally after adding a prompt
    /// anchored at it. Everything after it is dropped (its exits change epoch).
    /// The story ended on the beat on screen but the reader wants more: the ending
    /// becomes "continue" (the same beat, its words unchanged) and the next beat is
    /// written from there.
    pub fn keep_going(&self, play: &str) -> Result<()> {
        let _one = self.inner.edits.lock().expect("edits");
        let path = self.play(play)?.path;
        let last = path
            .last()
            .ok_or_else(|| EngineError::Invalid("nothing to continue".into()))?;
        let mut b = self
            .st()
            .graph
            .beats
            .get(&last.beat)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(last.beat.clone()))?;
        if !matches!(b.next, Next::End) {
            return Ok(());
        }
        b.next = Next::Continue;
        b.kept_going = true;
        self.write(Record::Beat(b.clone()))?;
        self.emit(Event::Beat { beat: b.id });
        self.wake();
        Ok(())
    }

    /// At a decision, the reader's own choice (in their words): added to the beat's
    /// choices (the others and anything written for them stay as they are). Returns its
    /// exit, to advance through.
    pub fn add_choice(&self, play: &str, text: &str) -> Result<usize> {
        // One at a time, read and written together: a beat saved in between would
        // otherwise drop the new choice ("no exit 2", 2026-11-21 alpha).
        let _one = self.inner.edits.lock().expect("edits");
        let text: String = text.trim().chars().take(300).collect();
        if text.is_empty() {
            return Err(EngineError::Invalid("write your choice first".into()));
        }
        let path = self.play(play)?.path;
        let last = path
            .last()
            .ok_or_else(|| EngineError::Invalid("no decision here".into()))?;
        let mut b = self
            .st()
            .graph
            .beats
            .get(&last.beat)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(last.beat.clone()))?;
        let allowed = Resolver::new(&self.st().graph, &path, None)
            .settings()
            .own_choices;
        let Next::Choices { choices } = &mut b.next else {
            return Err(EngineError::Invalid("no decision here".into()));
        };
        if !allowed {
            return Err(EngineError::Invalid(
                "this story doesn't take choices of your own".into(),
            ));
        }
        // The same words again: that choice.
        if let Some(i) = choices
            .iter()
            .position(|c| c.label.eq_ignore_ascii_case(&text))
        {
            return Ok(i);
        }
        choices.push(Choice {
            label: text,
            likely: None,
            own: true,
        });
        let exit = choices.len() - 1;
        self.write(Record::Beat(b.clone()))?;
        self.emit(Event::Beat { beat: b.id });
        Ok(exit)
    }

    pub fn rerun(&self, play: &str, prompt: Option<(String, Scope)>) -> Result<()> {
        let path = self.play(play)?.path;
        let last = path
            .last()
            .ok_or_else(|| EngineError::Invalid("nothing to re-run".into()))?;
        if let Some((text, scope)) = prompt {
            let id = if scope == Scope::Here {
                format!("here:{}", last.beat)
            } else {
                new_id("d")
            };
            self.set_prompt(&id, &text, Some(last.beat.clone()), scope)?;
        }
        let key = TaskKey::Rerun {
            beat: last.beat.clone(),
        };
        let parent = path[..path.len() - 1].to_vec();
        let mut st = self.st();
        st.wanted_paths.insert(key, parent);
        drop(st);
        self.wake();
        Ok(())
    }

    /// Edit mode: generate a new version of the current beat. The old one stays.
    pub fn fork(&self, play: &str, prompt: Option<String>) -> Result<()> {
        let path = self.play(play)?.path;
        if path.is_empty() {
            return Err(EngineError::Invalid("nothing on screen to fork".into()));
        }
        // The opening: another version of how the story begins (it has nothing before it).
        if path.len() == 1 {
            let key = TaskKey::Fork {
                from: String::new(),
                epoch: 0,
                exit: 0,
            };
            let mut st = self.st();
            if let Some(text) = prompt.filter(|t| !t.trim().is_empty()) {
                st.notes.insert(key.clone(), text);
            }
            st.wanted_paths.insert(key, vec![]);
            drop(st);
            self.wake();
            return Ok(());
        }
        let parent = path[..path.len() - 1].to_vec();
        let from = parent.last().expect("non-empty");
        let exit = from.exit.unwrap_or(0);
        if let Some(text) = prompt {
            self.set_prompt(
                &new_id("d"),
                &text,
                Some(from.beat.clone()),
                Scope::FromHere,
            )?;
        }
        let mut st = self.st();
        let epoch = st.graph.beats.get(&from.beat).map(|b| b.epoch).unwrap_or(0);
        st.wanted_paths.insert(
            TaskKey::Fork {
                from: from.beat.clone(),
                epoch,
                exit,
            },
            parent,
        );
        drop(st);
        self.wake();
        Ok(())
    }

    /// Switch the current beat to another version (fork navigation).
    pub fn switch_version(&self, play: &str, version: &str) -> Result<()> {
        let mut path = self.play(play)?.path;
        // The opening's versions.
        if path.len() == 1 {
            if !self.st().graph.openings.iter().any(|v| v == version) {
                return Err(EngineError::NotFound(version.into()));
            }
            path[0] = Hop {
                beat: version.into(),
                exit: None,
            };
            self.set_path(play, path)?;
            self.wake();
            return Ok(());
        }
        if path.len() < 2 {
            return Err(EngineError::Invalid("no versions here".into()));
        }
        let n = path.len();
        let from = &path[n - 2];
        let ok = self
            .st()
            .graph
            .versions(&from.beat, from.exit.unwrap_or(0))
            .iter()
            .any(|v| v == version);
        if !ok {
            return Err(EngineError::NotFound(version.into()));
        }
        path[n - 1] = Hop {
            beat: version.into(),
            exit: None,
        };
        self.set_path(play, path)?;
        self.wake();
        Ok(())
    }

    // Saves.

    /// Save a playthrough's place, in `slot` of the save screen if given (replacing
    /// whatever was saved there), at `line` of its beat (the first if not given).
    pub fn save(
        &self,
        play: &str,
        name: &str,
        slot: Option<u32>,
        line: Option<u32>,
    ) -> Result<String> {
        let p = self.play(play)?;
        if let Some(n) = slot {
            let taken: Vec<String> = self
                .st()
                .plays
                .saves
                .values()
                .filter(|s| !s.deleted && s.slot == Some(n))
                .map(|s| s.id.clone())
                .collect();
            for id in taken {
                self.delete_save(&id)?;
            }
        }
        let (thumb, excerpt, line, choices) = {
            let st = self.st();
            let thumb = showing_at(&st.graph, &p.path).map(|m| m.id.clone());
            let b = p.path.last().and_then(|h| st.graph.beats.get(&h.beat));
            let n = b.map_or(0, |b| b.lines.len());
            let line = line.map(|i| i.min(n.saturating_sub(1) as u32));
            let i = line.unwrap_or(0) as usize;
            let excerpt = b
                .and_then(|b| b.lines.get(i))
                .map(|l| l.text.chars().take(120).collect())
                .unwrap_or_default();
            // At the decision (its last line): the buttons were on screen too.
            let choices = match b.map(|b| &b.next) {
                Some(Next::Choices { choices }) if i + 1 >= n && line.is_some() => {
                    choices.iter().map(|c| c.label.clone()).collect()
                }
                _ => Vec::new(),
            };
            (thumb, excerpt, line, choices)
        };
        let s = Save {
            id: new_id("s"),
            name: name.into(),
            play: play.into(),
            path: p.path,
            thumb,
            excerpt,
            line,
            choices,
            slot,
            at: now_ms(),
            deleted: false,
        };
        let id = s.id.clone();
        self.write_play(PlayRecord::Save(s))?;
        Ok(id)
    }

    pub fn saves(&self) -> Vec<Save> {
        let mut v: Vec<Save> = self.st().plays.saves.values().cloned().collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.at));
        v
    }

    pub fn delete_save(&self, id: &str) -> Result<()> {
        let mut s = self
            .st()
            .plays
            .saves
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(id.into()))?;
        s.deleted = true;
        self.write_play(PlayRecord::Save(s))
    }

    /// Load a save into a playthrough.
    pub fn load(&self, save: &str, play: &str) -> Result<()> {
        let s = self
            .st()
            .plays
            .saves
            .get(save)
            .cloned()
            .ok_or_else(|| EngineError::NotFound(save.into()))?;
        let valid = validate_path(&self.st().graph, &s.path);
        let path = if valid {
            s.path
        } else {
            // The story changed under the save: keep the longest prefix that still holds.
            let st = self.st();
            let mut p: Path = Vec::new();
            for h in s.path {
                let mut cand = p.clone();
                cand.push(Hop {
                    beat: h.beat.clone(),
                    exit: None,
                });
                if !validate_path(&st.graph, &cand) {
                    break;
                }
                p.push(h);
            }
            if let Some(l) = p.last_mut() {
                l.exit = None;
            }
            p
        };
        self.activate(play);
        self.set_path(play, path)
    }

    pub fn prefs(&self) -> Value {
        self.st().plays.prefs.clone()
    }

    pub fn set_prefs(&self, prefs: Value) -> Result<()> {
        self.write_play(PlayRecord::Prefs { prefs })
    }

    // Views.

    pub fn play_view(&self, play: &str) -> Result<PlayView> {
        let p = self.play(play)?;
        let st = self.st();
        let favor = st.active.get(play).and_then(|a| a.favor);
        let fresh = st.active.get(play).and_then(|a| a.fresh.clone());
        // Lines of the beat this player is waiting on, as they're written.
        let draft = st
            .active
            .get(play)
            .and_then(|a| a.waiting)
            .and_then(|exit| {
                let from = p.path.last().map(|h| h.beat.clone()).unwrap_or_default();
                st.drafts.get(&(from, exit)).cloned()
            });
        // Being written: taken by a worker, not just waiting its turn (then it's "up
        // next"; 2026-11-21 alpha: queued text showed "writing" for minutes).
        let busy = |k: &TaskKey| {
            st.tasks.get(k).is_some_and(|t| {
                t.began
                    || t.job
                        .as_ref()
                        .is_some_and(|j| self.inner.backend.started(j))
            })
        };
        let mut v = view::play_view(
            &st.graph,
            &st.plays,
            &p,
            favor,
            &|from, epoch, exit| {
                busy(&TaskKey::Gen {
                    from: from.into(),
                    epoch,
                    exit,
                }) || busy(&TaskKey::Fork {
                    from: from.into(),
                    epoch,
                    exit,
                })
                    // Written already but being rewritten (after a steer or a setup
                    // change): that's writing too (2026-11-22 alpha: it said "up next…"
                    // for the whole rewrite).
                    || st.graph.follow(from, exit, None).is_some_and(|c| {
                        busy(&TaskKey::Rerun {
                            beat: c.id.clone(),
                        })
                    })
            },
            &|beat| busy(&TaskKey::Rerun { beat: beat.into() }),
        );
        v.draft = draft;
        // Before the opening shows: writing it, then painting its picture.
        if p.path.is_empty() {
            v.opening = Some(match &st.graph.root {
                None => (
                    "writing".into(),
                    text_progress(
                        &st,
                        &TaskKey::Gen {
                            from: String::new(),
                            epoch: 0,
                            exit: 0,
                        },
                    ),
                ),
                // Being rewritten for a changed setup: that's writing, not painting.
                Some(r)
                    if opening_stale(&st.graph)
                        || st.tasks.contains_key(&TaskKey::Rerun { beat: r.clone() }) =>
                {
                    (
                        "writing".into(),
                        text_progress(&st, &TaskKey::Rerun { beat: r.clone() }),
                    )
                }
                // Its picture made, its voices and sounds being made.
                Some(r)
                    if {
                        let b = &st.graph.beats[r];
                        let shown = match &b.show {
                            Show::Keep => true,
                            Show::New { media } | Show::Reuse { media } => {
                                media_settled(&st.graph, media)
                            }
                        };
                        shown && sound_pending(&st.graph, b)
                    } =>
                {
                    (
                        "voicing".into(),
                        view::audio_done(&st.graph, &st.graph.beats[r]),
                    )
                }
                Some(r) => match &st.graph.beats[r].show {
                    Show::New { media } | Show::Reuse { media } => {
                        let video = st
                            .graph
                            .media
                            .get(media)
                            .is_some_and(|m| m.kind == MediaKind::Video);
                        let pr = st
                            .tasks
                            .get(&TaskKey::Media {
                                media: media.clone(),
                            })
                            .and_then(|t| t.job.as_ref())
                            .and_then(|j| self.inner.backend.progress(j));
                        ((if video { "filming" } else { "painting" }).into(), pr)
                    }
                    Show::Keep => ("writing".into(), None),
                },
            });
        }
        // How far each next screen is: its beat as it's written (estimated from how much
        // of a usual reply has streamed), then its picture or video (from the backend).
        if let Some(b) = v.beat.as_mut() {
            // The rewrite of the beat already behind an exit, if one's under way.
            let rerun_key = |exit: usize| -> Option<TaskKey> {
                let c = st.graph.follow(&b.id, exit, None)?;
                let k = TaskKey::Rerun { beat: c.id.clone() };
                st.tasks.contains_key(&k).then_some(k)
            };
            let writing = |exit: usize| -> Option<f32> {
                text_progress(
                    &st,
                    &TaskKey::Gen {
                        from: b.id.clone(),
                        epoch: b.epoch,
                        exit,
                    },
                )
                .or_else(|| text_progress(&st, &rerun_key(exit)?))
            };
            let progress = |exit: usize| -> Option<f32> {
                let c = st.graph.follow(&b.id, exit, None)?;
                if let Show::New { media } | Show::Reuse { media } = &c.show
                    && !media_settled(&st.graph, media)
                {
                    let t = st.tasks.get(&TaskKey::Media {
                        media: media.clone(),
                    })?;
                    return self.inner.backend.progress(t.job.as_ref()?);
                }
                // Its picture made: how many of its voices and sounds are.
                view::audio_done(&st.graph, c)
            };
            // One progress for the whole screen, writing then painting (2026-11-21 alpha:
            // "writing" then "painting" as separate stages felt weird).
            let whole = |painting: bool,
                         generating: bool,
                         paint: &dyn Fn() -> Option<f32>,
                         write: &dyn Fn() -> Option<f32>|
             -> Option<f32> {
                // One ring per stage, each 0..1 (a combined ring looked stuck halfway:
                // writing is quick, painting slow; 2026-11-21 alpha).
                if painting {
                    paint()
                } else if generating {
                    write()
                } else {
                    None
                }
            };
            // The job making the screen behind an exit: its text, else its picture.
            let making = |exit: usize| -> Option<JobId> {
                let job = |k: &TaskKey| st.tasks.get(k).and_then(|t| t.job.clone());
                let gen_key = TaskKey::Gen {
                    from: b.id.clone(),
                    epoch: b.epoch,
                    exit,
                };
                if let Some(j) = rerun_key(exit).and_then(|k| job(&k)) {
                    return Some(j);
                }
                let j = match st.graph.follow(&b.id, exit, None) {
                    None => job(&gen_key),
                    Some(c) => match &c.show {
                        Show::New { media } | Show::Reuse { media } => job(&TaskKey::Media {
                            media: media.clone(),
                        }),
                        Show::Keep => None,
                    },
                }?;
                Some(j)
            };
            // Waiting its turn in the backend's line.
            let in_line = |exit: usize| making(exit).and_then(|j| self.inner.backend.queued(&j));
            // Started, but the backend is still loading its model.
            let loading =
                |exit: usize| making(exit).is_some_and(|j| self.inner.backend.loading(&j));
            // Failed, not running, and waiting to be tried again: its text or its picture.
            let now = std::time::Instant::now();
            let backing_off = |k: &TaskKey| {
                !st.tasks.contains_key(k) && st.failures.get(k).is_some_and(|f| now < f.1)
            };
            let retrying = |exit: usize| -> bool {
                let gen_key = TaskKey::Gen {
                    from: b.id.clone(),
                    epoch: b.epoch,
                    exit,
                };
                match st.graph.follow(&b.id, exit, None) {
                    None => backing_off(&gen_key),
                    Some(c) => {
                        backing_off(&TaskKey::Rerun { beat: c.id.clone() })
                            || matches!(&c.show, Show::New { media } | Show::Reuse { media }
                                if backing_off(&TaskKey::Media { media: media.clone() }))
                    }
                }
            };
            match &mut b.next {
                view::NextView::Continue {
                    painting,
                    voicing,
                    generating,
                    progress: p,
                    retrying: r,
                    line: l,
                    loading: ld,
                    ..
                } => {
                    *r = !*generating && retrying(0);
                    *l = in_line(0);
                    *ld = loading(0);
                    *p = whole(*painting || *voicing, *generating, &|| progress(0), &|| {
                        writing(0)
                    });
                }
                view::NextView::Choices { choices } => {
                    for (i, c) in choices.iter_mut().enumerate() {
                        c.retrying = !c.generating && retrying(i);
                        c.line = in_line(i);
                        c.loading = loading(i);
                        c.progress = whole(
                            c.painting || c.voicing,
                            c.generating,
                            &|| progress(i),
                            &|| writing(i),
                        );
                    }
                }
                _ => {}
            }
        }
        // "Seen" means seen before this visit, so skipping stops on new text.
        if let Some(b) = v.beat.as_mut()
            && fresh.as_ref() == Some(&b.id)
        {
            b.seen = false;
        }
        Ok(v)
    }

    /// What an export would leave out.
    pub fn export_report(&self) -> view::ExportReport {
        view::export_report(&self.st().graph)
    }

    pub fn beat_view(&self, beat: &str) -> Result<BeatView> {
        let st = self.st();
        let b = st
            .graph
            .beats
            .get(beat)
            .ok_or_else(|| EngineError::NotFound(beat.into()))?;
        Ok(view::beat_view(&st.graph, &st.plays, b, &[], &|_, _, _| {
            false
        }))
    }

    pub fn map(&self, play: Option<&str>) -> MapView {
        let path = play
            .and_then(|p| self.play(p).ok())
            .map(|p| p.path)
            .unwrap_or_default();
        let st = self.st();
        let generating: HashSet<(BeatId, usize)> = st
            .tasks
            .keys()
            .chain(st.wanted_paths.keys())
            .filter_map(|k| match k {
                TaskKey::Gen { from, exit, .. } | TaskKey::Fork { from, exit, .. } => {
                    Some((from.clone(), *exit))
                }
                _ => None,
            })
            .collect();
        view::map_view(&st.graph, &st.plays, &path, &generating)
    }

    pub fn setup_view(&self, play: Option<&str>) -> view::SetupView {
        let path = play
            .and_then(|p| self.play(p).ok())
            .map(|p| p.path)
            .unwrap_or_default();
        let st = self.st();
        view::setup_view(&st.graph, &path)
    }

    // Export.

    /// Write an export bundle of every non-pending path.
    pub fn export<W: std::io::Write + std::io::Seek>(
        &self,
        install: &yarner_format::Install,
        passphrase: &str,
        out: W,
    ) -> Result<(W, view::ExportReport)> {
        let (records, blobs, report) = {
            let st = self.st();
            let g = &st.graph;
            let records: Vec<Vec<u8>> = g
                .records(true)
                .iter()
                .map(|r| serde_json::to_vec(r).expect("json"))
                .collect();
            let keep: HashSet<BeatId> = g
                .reachable()
                .into_iter()
                .filter(|id| !g.beats[id].pending)
                .collect();
            let used = g.used_media(&keep);
            let mut blobs: Vec<String> = used
                .iter()
                .filter_map(|m| g.media.get(m)?.blob.clone())
                .collect();
            blobs.sort();
            blobs.dedup();
            (records, blobs, view::export_report(g))
        };
        let out = yarner_format::bundle::export(
            install,
            &self.inner.story,
            records.iter().map(Vec::as_slice),
            &blobs,
            passphrase,
            out,
        )?;
        Ok((out, report))
    }

    /// Start `target` (a new, empty story) with this story's setup as it applies
    /// everywhere: its prompts, objects (with their reference pictures) and settings. No
    /// beats, plays or other pictures. Directions given from play are anchored at beats,
    /// so they stay behind.
    pub fn copy_setup_to(&self, target: &Story) -> Result<()> {
        let (prompts, objects, settings, media) = {
            let st = self.st();
            let g = &st.graph;
            let res = Resolver::new(g, &[], None);
            let prompts: Vec<PromptRev> = res.prompts().into_iter().cloned().collect();
            let objects: Vec<ObjectRev> = res.objects().into_iter().cloned().collect();
            let settings = res.settings();
            // The objects' reference pictures and the rest of the setup library (pictures
            // uploaded or made in setup, not for a beat).
            let mut ids: Vec<&MediaId> = objects
                .iter()
                .flat_map(|o| o.refs.iter().map(|r| &r.media))
                .chain(g.media.values().filter(|m| m.beat.is_none()).map(|m| &m.id))
                .collect();
            ids.sort();
            ids.dedup();
            let media: Vec<Media> = ids
                .into_iter()
                .filter_map(|id| g.media.get(id))
                .filter(|m| m.status == MediaStatus::Ready && m.blob.is_some())
                .cloned()
                .collect();
            (prompts, objects, settings, media)
        };
        let kept: HashSet<MediaId> = media.iter().map(|m| m.id.clone()).collect();
        let mut records: Vec<Record> = Vec::new();
        records.push(Record::Settings(SettingsRev {
            rev: new_id("r"),
            anchor: None,
            settings,
            at: now_ms(),
        }));
        for mut m in media {
            let bytes = self
                .inner
                .story
                .read_blob(m.blob.as_deref().expect("blob"))?;
            m.blob = Some(target.put_blob(&bytes)?);
            m.beat = None;
            m.refs.retain(|r| kept.contains(r));
            records.push(Record::Media(m));
        }
        records.extend(prompts.into_iter().map(|mut p| {
            p.anchor = None;
            Record::Prompt(p)
        }));
        records.extend(objects.into_iter().map(|mut o| {
            o.anchor = None;
            o.refs.retain(|r| kept.contains(&r.media));
            // A voice keeps its clips that came along (one still being made starts over).
            if let Some(v) = o.voice.as_mut() {
                for d in &mut v.deliveries {
                    d.clip = d.clip.take().filter(|c| kept.contains(c));
                }
            }
            Record::Object(o)
        }));
        let bytes: Vec<zeroize::Zeroizing<Vec<u8>>> = records
            .iter()
            .map(|r| zeroize::Zeroizing::new(serde_json::to_vec(r).expect("json")))
            .collect();
        target.compact_graph(bytes.iter().map(|b| b.as_slice()))?;
        Ok(())
    }

    // Storage.

    fn write(&self, r: Record) -> Result<()> {
        let bytes = zeroize::Zeroizing::new(serde_json::to_vec(&r).expect("json"));
        let mut st = self.st();
        st.log.append(&bytes)?;
        st.appended += 1;
        if matches!(
            r,
            Record::Prompt(_) | Record::Object(_) | Record::Settings(_) | Record::Retire { .. }
        ) {
            st.setup_at = std::time::Instant::now();
        }
        st.graph.apply(r);
        if st.appended >= self.inner.cfg.compact_after {
            let recs: Vec<Vec<u8>> = st
                .graph
                .records(false)
                .iter()
                .map(|r| serde_json::to_vec(r).expect("json"))
                .collect();
            st.log = self
                .inner
                .story
                .compact_graph(recs.iter().map(Vec::as_slice))?;
            st.appended = recs.len();
        }
        Ok(())
    }

    fn write_play(&self, r: PlayRecord) -> Result<()> {
        let bytes = zeroize::Zeroizing::new(serde_json::to_vec(&r).expect("json"));
        let mut st = self.st();
        st.plays_log.append(&bytes)?;
        st.plays.apply(r);
        Ok(())
    }

    /// Compact the graph log now.
    pub fn compact(&self) -> Result<()> {
        let mut st = self.st();
        let recs: Vec<Vec<u8>> = st
            .graph
            .records(false)
            .iter()
            .map(|r| serde_json::to_vec(r).expect("json"))
            .collect();
        st.log = self
            .inner
            .story
            .compact_graph(recs.iter().map(Vec::as_slice))?;
        st.appended = recs.len();
        Ok(())
    }
}

/// Any path from an opening (the root or a forked one) to `beat` (breadth first, so the
/// shortest).
fn find_path(g: &Graph, target: &str) -> Option<Path> {
    let starts: Vec<BeatId> = if g.openings.is_empty() {
        vec![g.root.clone()?]
    } else {
        g.openings.clone()
    };
    let mut prev: HashMap<BeatId, (BeatId, usize)> = HashMap::new();
    let mut q: VecDeque<BeatId> = starts.iter().cloned().collect();
    let mut seen: HashSet<BeatId> = starts.into_iter().collect();
    while let Some(id) = q.pop_front() {
        if id == target {
            let mut path = vec![Hop {
                beat: id.clone(),
                exit: None,
            }];
            let mut cur = id;
            while let Some((p, e)) = prev.get(&cur) {
                path.push(Hop {
                    beat: p.clone(),
                    exit: Some(*e),
                });
                cur = p.clone();
            }
            path.reverse();
            return Some(path);
        }
        let Some(b) = g.beats.get(&id) else { continue };
        for exit in 0..b.next.exits() {
            for v in g.versions(&id, exit) {
                if seen.insert(v.clone()) {
                    prev.insert(v.clone(), (id.clone(), exit));
                    q.push_back(v.clone());
                }
            }
        }
    }
    None
}

// Scheduling.

#[derive(Clone)]
struct Want {
    key: TaskKey,
    kind: JobKind,
    priority: Priority,
    path: Path,
    /// Media for a screen a player can reach in one step: their next screen isn't ready
    /// without it, so it isn't held back by the media gate.
    soon: bool,
}

/// Everything worth doing now, from the graph and the active playthroughs.
fn plan(st: &State, cfg: &Config, depth: usize) -> Vec<Want> {
    let g = &st.graph;
    let mut wants: HashMap<TaskKey, Want> = HashMap::new();
    let mut add = |w: Want| {
        let e = wants.entry(w.key.clone());
        match e {
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if o.get().priority < w.priority {
                    o.insert(w);
                }
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(w);
            }
        }
    };

    // Explicit edit requests come first: someone is looking at them.
    for (k, path) in &st.wanted_paths {
        add(Want {
            key: k.clone(),
            kind: JobKind::Text,
            priority: prio::WAITING,
            path: path.clone(),
            soon: false,
        });
    }

    // Setup pictures and voices being designed.
    for m in g.media.values() {
        if m.status == MediaStatus::Requested && m.beat.is_none() {
            add(Want {
                key: TaskKey::Media {
                    media: m.id.clone(),
                },
                kind: job_kind(m),
                priority: prio::SETUP_MEDIA,
                path: vec![],
                soon: false,
            });
        }
    }

    // The opening: written ahead while the author is still setting up, and rewritten if
    // the setup changes before anyone sees it, so Start shows it at once.
    let starting = st
        .active
        .keys()
        .filter_map(|p| st.plays.plays.get(p))
        .any(|p| p.path.is_empty());
    let settled = cfg
        .prefetch_after_secs
        .is_some_and(|s| st.setup_at.elapsed().as_secs_f64() >= s);
    let has_story = Resolver::new(g, &[], None).prompt(STORY_PROMPT).is_some();
    // A stale opening a waiting reader takes as it is isn't rewritten under them.
    let stale_root = opening_stale(g)
        && !st.active.iter().any(|(p, _)| {
            st.plays.plays.get(p).is_some_and(|x| x.path.is_empty()) && opening_theirs(st, p)
        });
    match &g.root {
        None if settled && has_story && !starting => add(Want {
            key: TaskKey::Gen {
                from: String::new(),
                epoch: 0,
                exit: 0,
            },
            kind: JobKind::Text,
            priority: prio::PREFETCH,
            path: vec![],
            soon: false,
        }),
        Some(r) if stale_root && (settled || starting) => add(Want {
            key: TaskKey::Rerun { beat: r.clone() },
            kind: JobKind::Text,
            priority: if starting {
                prio::WAITING
            } else {
                prio::PREFETCH
            },
            path: vec![],
            soon: false,
        }),
        Some(r) if g.beats[r].pending && !stale_root && settled => {
            media_want(g, &g.beats[r], prio::PREFETCH, true, &mut add);
        }
        _ => {}
    }

    // When pictures can pause, every one ahead starts as soon as its beat exists (a
    // reader's text pauses it, losing nothing; 2026-11-21 alpha: 85 s pictures only
    // started when their screen was next, so readers waited on them).
    let ungated = if pausable(st, JobKind::Image) {
        usize::MAX
    } else {
        0
    };
    for (pid, active) in &st.active {
        let Some(play) = st.plays.plays.get(pid) else {
            continue;
        };
        if play.path.is_empty() {
            if stale_root {
                continue;
            }
            if g.root.is_none() {
                add(Want {
                    key: TaskKey::Gen {
                        from: String::new(),
                        epoch: 0,
                        exit: 0,
                    },
                    kind: JobKind::Text,
                    priority: prio::WAITING,
                    path: vec![],
                    soon: false,
                });
            } else if let Some(root) = &g.root {
                // The opening isn't shown until its picture is painted.
                media_want(g, &g.beats[root], prio::WAITING, true, &mut add);
                walk_ahead(
                    g,
                    vec![Hop {
                        beat: root.clone(),
                        exit: None,
                    }],
                    0,
                    depth,
                    Front::default(),
                    ungated,
                    &mut add,
                );
            }
            continue;
        }
        // What's on screen now (possibly held from an earlier beat), then this beat's own.
        if let Some(m) = showing_at(g, &play.path)
            && let Some(b) = m.beat.as_ref().and_then(|b| g.beats.get(b))
        {
            media_want(g, b, prio::MEDIA_NOW, true, &mut add);
        }
        if let Some(last) = play.path.last()
            && let Some(b) = g.beats.get(&last.beat)
        {
            media_want(g, b, prio::MEDIA_NOW, true, &mut add);
        }
        walk_ahead(
            g,
            play.path.clone(),
            0,
            depth,
            Front {
                favor: active.favor,
                waiting: active.waiting,
            },
            ungated,
            &mut add,
        );
    }
    // Backlog: media of beats a player already passed (needed on the way back and for
    // export), behind everything else.
    for m in g.media.values() {
        if m.status == MediaStatus::Requested
            && let Some(b) = m.beat.as_ref().and_then(|b| g.beats.get(b))
            && !b.pending
        {
            media_want(g, b, prio::BACKLOG, false, &mut add);
        }
    }
    let mut v: Vec<Want> = wants.into_values().collect();
    v.sort_by_key(|w| std::cmp::Reverse(w.priority));
    v
}

/// How far a text job is, 0..1: first the model reading what it's given (estimated from
/// how long reading took lately), then the reply as it streams (estimated from how long
/// replies run). `None` until a worker takes it.
fn text_progress(st: &State, k: &TaskKey) -> Option<f32> {
    let t = st.tasks.get(k)?;
    t.job.as_ref()?;
    const READ: f64 = 0.35;
    let streamed = match k {
        TaskKey::Gen { from, exit, .. } | TaskKey::Fork { from, exit, .. } => {
            st.written.get(&(from.clone(), *exit)).copied().unwrap_or(0)
        }
        _ => 0,
    };
    if streamed > 0 {
        let r = (streamed as f64 / st.reply_chars.max(1.0)).min(0.95);
        return Some((READ + (1.0 - READ) * r) as f32);
    }
    let recent: Vec<f64> = st
        .text_stats
        .iter()
        .rev()
        .take(20)
        .map(|s| s.prefill_secs)
        .collect();
    let read = if recent.is_empty() {
        8.0
    } else {
        recent.iter().sum::<f64>() / recent.len() as f64
    }
    .max(1.0);
    Some((READ * (t.started.elapsed().as_secs_f64() / read).min(0.95)) as f32)
}

/// The opening was written since this reader pressed Start: it's theirs even if the setup
/// moved on while it was written (each edit during a rewrite made it stale again, so a
/// reader could wait through rewrite after rewrite: 2026-11-21 alpha, 86 s).
fn opening_theirs(st: &State, play: &str) -> bool {
    let Some(root) = st.graph.root.as_ref().and_then(|r| st.graph.beats.get(r)) else {
        return false;
    };
    st.active
        .get(play)
        .and_then(|a| a.waiting_since)
        .map(|t| now_ms().saturating_sub(t.elapsed().as_millis() as u64))
        .is_some_and(|asked| root.at >= asked)
}

/// The opening was written ahead and nobody has seen it, but the setup has changed since.
fn opening_stale(g: &Graph) -> bool {
    g.root.as_ref().is_some_and(|r| {
        g.beats[r].pending
            && is_stale(
                g,
                &[Hop {
                    beat: r.clone(),
                    exit: None,
                }],
            )
    })
}

/// The way to rewrite a stale beat along: the way it was first written (its staleness is
/// judged that way too: see `is_stale`), so one rewrite settles it.
fn rerun_path(g: &Graph, beat: &str, fallback: Path) -> Path {
    crate::model::origin_path(g, beat)
        .map(|mut p| {
            p.pop();
            p
        })
        .filter(|p| !p.is_empty())
        .unwrap_or(fallback)
}

/// The kind of job that makes a piece of media.
fn job_kind(m: &Media) -> JobKind {
    match m.kind {
        MediaKind::Image => JobKind::Image,
        MediaKind::Video => JobKind::Video,
        MediaKind::Audio => JobKind::Audio,
    }
}

/// Want what beat `b` puts on screen, says and plays, if it isn't made yet (`soon`: see
/// [`Want`]).
fn media_want(g: &Graph, b: &Beat, priority: Priority, soon: bool, add: &mut impl FnMut(Want)) {
    let mut want = |id: &MediaId, priority: Priority| {
        let Some(m) = g.media.get(id) else {
            return;
        };
        if m.status != MediaStatus::Requested {
            return;
        }
        add(Want {
            key: TaskKey::Media {
                media: m.id.clone(),
            },
            kind: job_kind(m),
            priority,
            path: vec![],
            // Speculative video (a branch the reader may never take) waits its turn like
            // any far-off picture: once started it can't be stopped for minutes.
            soon,
        });
    };
    if let Show::New { media } | Show::Reuse { media } = &b.show {
        want(media, priority);
    }
    // Its voices and sounds, right after its picture: the screen comes whole with them.
    for id in b.audio() {
        want(id, priority - 2);
    }
}

/// What the player at the end of a path favors and waits on (exits of its last beat).
#[derive(Clone, Copy, Default)]
struct Front {
    favor: Option<usize>,
    waiting: Option<usize>,
}

/// From the end of `path`, want every missing beat and requested media up to `max` beats
/// ahead.
fn walk_ahead(
    g: &Graph,
    path: Path,
    depth: usize,
    max: usize,
    front: Front,
    ungated: usize,
    add: &mut impl FnMut(Want),
) {
    if depth >= max {
        return;
    }
    let last = path.last().expect("non-empty");
    let Some(b) = g.beats.get(&last.beat) else {
        return;
    };
    let base = prio::NEXT - depth as Priority * prio::DEPTH_STEP;
    let exits = b.next.exits();
    // Every choice of the beat on screen gets its next beat; further ahead, only the most
    // likely line (speculating every branch multiplies the work per choice).
    let main = match &b.next {
        Next::Choices { choices } => choices
            .iter()
            .enumerate()
            .max_by(|a, b| {
                let l = |c: &Choice| c.likely.unwrap_or(0.0);
                l(a.1).total_cmp(&l(b.1)).then(b.0.cmp(&a.0))
            })
            .map_or(0, |(i, _)| i),
        _ => 0,
    };
    for exit in 0..exits {
        if depth > 0 && exit != main {
            continue;
        }
        let mut p = match &b.next {
            Next::Choices { choices } => {
                let likely = choices[exit]
                    .likely
                    .unwrap_or(1.0 / exits as f32)
                    .clamp(0.0, 1.0);
                prio::CHOICE - depth as Priority * prio::DEPTH_STEP + (likely * 100.0) as Priority
            }
            _ => base,
        };
        // The screens right after the one on screen come before anything further on:
        // ordered among themselves by how likely each is, a favored one first.
        if depth == 0 {
            p = match &b.next {
                Next::Choices { choices } => {
                    let likely = choices[exit]
                        .likely
                        .unwrap_or(1.0 / exits as f32)
                        .clamp(0.0, 1.0);
                    // At or above NOW (all of them jump the line), under WAITING.
                    prio::NOW + (likely * 40.0) as Priority
                }
                _ => prio::NOW,
            };
        }
        if depth == 0 && front.favor == Some(exit) {
            p = prio::WAITING - 1;
        }
        if depth == 0 && front.waiting == Some(exit) {
            p = prio::WAITING;
        }
        let mut next_path = path.clone();
        next_path.last_mut().expect("non-empty").exit = Some(exit);
        match g.follow(&b.id, exit, None) {
            None => add(Want {
                key: TaskKey::Gen {
                    from: b.id.clone(),
                    epoch: b.epoch,
                    exit,
                },
                kind: JobKind::Text,
                priority: p,
                path: next_path,
                soon: false,
            }),
            Some(child)
                if child.pending && {
                    let mut with = next_path.clone();
                    with.push(Hop {
                        beat: child.id.clone(),
                        exit: None,
                    });
                    is_stale(g, &with)
                } =>
            {
                // Written ahead, never seen, and the setup changed since (a character
                // or prompt edited): rewritten with the change before anyone sees it.
                add(Want {
                    key: TaskKey::Rerun {
                        beat: child.id.clone(),
                    },
                    kind: JobKind::Text,
                    priority: p,
                    path: rerun_path(g, &child.id, next_path),
                    soon: false,
                });
            }
            Some(child) => {
                // Its screen isn't ready until its picture is: painted right after its
                // text, ahead of anything further on. The next screen's always skips the
                // media gate; up to `ungated` screens ahead do when pictures can pause
                // (a picture takes longer than a beat of reading, so starting it only
                // once its screen is next is too late; but one that can't pause would
                // block or be cancelled for the text a reader needs: 2026-11-21 mock
                // tune, 24% -> 28% waiting when ungated without pausing).
                let mp = if p >= prio::WAITING { p } else { p - 10 };
                media_want(g, child, mp, depth <= ungated, add);
                if path.iter().any(|h| h.beat == child.id) {
                    continue;
                }
                next_path.push(Hop {
                    beat: child.id.clone(),
                    exit: None,
                });
                walk_ahead(g, next_path, depth + 1, max, Front::default(), ungated, add);
            }
        }
    }
}

/// Every reader is away: nobody waiting, nobody moved for `away_secs`.
fn all_away(st: &State, cfg: &Config) -> bool {
    st.active.iter().all(|(p, a)| {
        // Waiting on a screen that's ready since is waiting on nothing: they move on with
        // their own tap, and until then they're as away as anyone (2026-11-23 alpha: a
        // reader who asked early and walked off held "keep generating" off for 26 min).
        let waits = a.waiting.is_some_and(|exit| {
            let Some(h) = st.plays.plays.get(p).and_then(|pl| pl.path.last()) else {
                return true;
            };
            !st.graph
                .follow(&h.beat, exit, None)
                .is_some_and(|c| crate::model::screen_ready(&st.graph, c))
        });
        !waits && a.last_move.elapsed().as_secs_f64() > cfg.away_secs
    })
}

/// What to run: what readers need (`schedule_near`); with "keep generating" on and
/// everyone away, once that's all done, the story filled out further (`fill_wide`).
fn schedule(st: &State, cfg: &Config) -> Vec<Want> {
    let near = schedule_near(st, cfg);
    if near.is_empty() && st.fill && !st.active.is_empty() && all_away(st, cfg) {
        return fill_wide(st, 4);
    }
    near
}

/// Below anything a reader is near (deep speculation goes negative; ORPHAN is -100).
const FILL_TOP: Priority = -110;

/// The story filled out from where each reader is: best first by how likely the path
/// there is (a choice's `likely`, multiplied along the way), so it spreads across choices
/// and goes deep at once, with no depth limit. Each screen whole: its beat, then its
/// picture or video. Endings end a line (nothing past them). Up to `n` wants.
fn fill_wide(st: &State, n: usize) -> Vec<Want> {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;
    struct Node {
        score: f64,
        path: Path,
    }
    impl PartialEq for Node {
        fn eq(&self, o: &Self) -> bool {
            self.score == o.score
        }
    }
    impl Eq for Node {}
    impl PartialOrd for Node {
        fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
            Some(self.cmp(o))
        }
    }
    impl Ord for Node {
        fn cmp(&self, o: &Self) -> Ordering {
            self.score.total_cmp(&o.score)
        }
    }
    let g = &st.graph;
    let mut heap = BinaryHeap::new();
    let mut seen: HashSet<BeatId> = HashSet::new();
    for pid in st.active.keys() {
        if let Some(p) = st.plays.plays.get(pid)
            && let Some(last) = p.path.last()
            && seen.insert(last.beat.clone())
        {
            heap.push(Node {
                score: 1.0,
                path: p.path.clone(),
            });
        }
    }
    // Then the rest of the story: the choices behind each reader and every other line,
    // from the opening (below what's ahead of them, which goes first). Without this a
    // reader at or near an ending had nothing left below them and the GPU sat idle all
    // night with "keep generating" on (2026-11-23 alpha: idle 7.5 h of 8).
    for pid in st.active.keys() {
        if let Some(p) = st.plays.plays.get(pid)
            && let Some(first) = p.path.first()
            && seen.insert(first.beat.clone())
        {
            heap.push(Node {
                score: 0.5,
                path: vec![Hop {
                    beat: first.beat.clone(),
                    exit: None,
                }],
            });
        }
    }
    // (score, the beat whose session state it goes on from or belongs with, want)
    let mut out: Vec<(f64, BeatId, Want)> = Vec::new();
    // A pool to pick from: the likeliest, with the lines just written put first.
    let pool = n * 8;
    // Bounded: a huge story is still planned in a moment each tick.
    let mut visited = 0usize;
    while let Some(Node { score, path }) = heap.pop() {
        visited += 1;
        if out.len() >= pool || visited > 20_000 {
            break;
        }
        let last = path.last().expect("non-empty");
        let Some(b) = g.beats.get(&last.beat) else {
            continue;
        };
        let exits = b.next.exits();
        for exit in 0..exits {
            let likely = match &b.next {
                Next::Choices { choices } => choices[exit]
                    .likely
                    .unwrap_or(1.0 / exits as f32)
                    .clamp(0.01, 1.0) as f64,
                _ => 1.0,
            };
            let s = score * likely;
            let mut next_path = path.clone();
            next_path.last_mut().expect("non-empty").exit = Some(exit);
            let child = g.follow(&b.id, exit, None);
            match child {
                None => out.push((
                    s,
                    b.id.clone(),
                    Want {
                        key: TaskKey::Gen {
                            from: b.id.clone(),
                            epoch: b.epoch,
                            exit,
                        },
                        kind: JobKind::Text,
                        priority: 0,
                        path: next_path,
                        soon: false,
                    },
                )),
                Some(c) => {
                    let mut with = next_path.clone();
                    with.push(Hop {
                        beat: c.id.clone(),
                        exit: None,
                    });
                    if c.pending && is_stale(g, &with) {
                        out.push((
                            s,
                            b.id.clone(),
                            Want {
                                key: TaskKey::Rerun { beat: c.id.clone() },
                                kind: JobKind::Text,
                                priority: 0,
                                path: rerun_path(g, &c.id, next_path),
                                soon: false,
                            },
                        ));
                        continue;
                    }
                    media_want(g, c, 0, true, &mut |w| out.push((s, c.id.clone(), w)));
                    if seen.insert(c.id.clone()) {
                        heap.push(Node {
                            score: s,
                            path: with,
                        });
                    }
                }
            }
        }
    }
    // Going on from a line just written first: its session state is still kept, so only
    // the new turn is read (2026-11-22 alpha: jumping between lines, each beat re-read
    // 10-12k tokens, ~100 s instead of ~15 s). Then likeliest first (a screen's picture
    // right after its beat: same score, found later).
    let warmth = |b: &BeatId| st.warm.iter().rposition(|w| w == b).map_or(0, |i| i + 1);
    out.sort_by(|a, b| warmth(&b.1).cmp(&warmth(&a.1)).then(b.0.total_cmp(&a.0)));
    out.into_iter()
        .take(n)
        .enumerate()
        .map(|(i, (_, _, mut w))| {
            w.priority = FILL_TOP - i as Priority;
            w
        })
        .collect()
}

/// What to run: the plan at the current depth with media gated. If that leaves the GPU
/// with nothing (all text written, no media fits the buffer), write further ahead to grow
/// the buffer; only at `max_depth` does media run regardless.
fn schedule_near(st: &State, cfg: &Config) -> Vec<Want> {
    let text_running = st.tasks.values().any(|t| t.kind == JobKind::Text);
    let mut depth = st.depth;
    loop {
        let wants = plan(st, cfg, depth);
        let gated = gate_media(st, cfg, wants.clone());
        let busy = text_running || !gated.is_empty() || !st.tasks.is_empty();
        if busy {
            return gated;
        }
        // Nothing to do this close: write further ahead rather than leave the GPU idle
        // (the beats that ask for pictures then exist early).
        if wants.is_empty() && depth < cfg.max_depth.max(st.depth) {
            depth += 1;
            continue;
        }
        if wants.is_empty() {
            return gated;
        }
        if depth >= cfg.max_depth.max(st.depth) {
            // Out of text to write. Pictures may run; video only while nobody is reading.
            let away = all_away(st, cfg);
            return wants
                .into_iter()
                .filter(|w| w.kind != JobKind::Video || away)
                .collect();
        }
        depth += 1;
    }
}

/// Seconds a player needs to read a beat.
fn read_secs(cfg: &Config, b: &Beat) -> f64 {
    let chars: usize = b.lines.iter().map(|l| l.text.chars().count()).sum();
    chars as f64 / cfg.read_chars_per_sec + b.lines.len() as f64 * cfg.read_secs_per_line
}

/// Reading time already generated ahead of beat `from`: the worst case over its exits,
/// up to `depth` beats deep. An ending has unlimited buffer (nothing more is needed).
fn buffered(g: &Graph, cfg: &Config, from: &str, depth: usize) -> f64 {
    let Some(b) = g.beats.get(from) else {
        return 0.0;
    };
    if depth == 0 || b.next.exits() == 0 {
        return if b.next.exits() == 0 {
            f64::INFINITY
        } else {
            0.0
        };
    }
    (0..b.next.exits())
        .map(|exit| match g.follow(from, exit, None) {
            Some(c) => read_secs(cfg, c) + buffered(g, cfg, &c.id, depth - 1),
            None => 0.0,
        })
        .fold(f64::INFINITY, f64::min)
}

/// Text never waits behind media: media starts only when its expected duration fits in
/// the reading time already buffered ahead of every active player, since a long job can't
/// be taken back once running.
fn gate_media(st: &State, cfg: &Config, wants: Vec<Want>) -> Vec<Want> {
    let g = &st.graph;
    let buffer = st
        .active
        .keys()
        .filter_map(|p| st.plays.plays.get(p))
        .map(|p| match p.path.last() {
            // Half the current beat is still left to read, on average.
            Some(h) => {
                g.beats
                    .get(&h.beat)
                    .map(|b| read_secs(cfg, b) / 2.0)
                    .unwrap_or(0.0)
                    + buffered(g, cfg, &h.beat, 4)
            }
            None => 0.0,
        })
        .fold(f64::INFINITY, f64::min);
    let guess = |k: JobKind| match k {
        JobKind::Text => 4.0,
        JobKind::Image => 30.0,
        JobKind::Video => 200.0,
        JobKind::Audio => 8.0,
    };
    // A player looking at a blank screen (no finished picture anywhere on their path)
    // gets their current picture now, even at the cost of a short wait for text.
    let blank: HashSet<MediaId> = st
        .active
        .keys()
        .filter_map(|p| st.plays.plays.get(p))
        .filter(|p| {
            !p.path.iter().any(|h| {
                g.beats.get(&h.beat).is_some_and(|b| match &b.show {
                    Show::Reuse { media } | Show::New { media } => g
                        .media
                        .get(media)
                        .is_some_and(|m| m.status == MediaStatus::Ready),
                    Show::Keep => false,
                })
            })
        })
        .filter_map(|p| showing_at(g, &p.path).map(|m| m.id.clone()))
        .collect();
    wants
        .into_iter()
        .filter(|w| {
            w.kind == JobKind::Text
                || matches!(&w.key, TaskKey::Media { media } if blank.contains(media))
                // A screen one step away: it can't be shown without its picture.
                || w.soon
                || {
                    let est = st
                        .durations
                        .get(&w.kind)
                        .copied()
                        .unwrap_or_else(|| guess(w.kind));
                    est <= buffer
                }
        })
        .collect()
}

fn cap(cfg: &Config, k: JobKind) -> usize {
    match k {
        JobKind::Text => cfg.max_text,
        JobKind::Image => cfg.max_image,
        JobKind::Video => cfg.max_video,
        JobKind::Audio => cfg.max_audio,
    }
}

async fn scheduler(weak: Weak<Inner>, wake: Arc<Notify>) {
    let mut asked = std::time::Instant::now() - Duration::from_secs(60);
    loop {
        {
            let Some(inner) = weak.upgrade() else { return };
            let e = Engine { inner };
            // What the models can do, once the backend answers (asked again now and
            // then until it does).
            if e.st().models.is_empty() && asked.elapsed() > Duration::from_secs(30) {
                asked = std::time::Instant::now();
                let m = e.inner.backend.models().await;
                e.st().models = m;
            }
            tick(&e).await;
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), wake.notified()).await;
    }
}

/// A reader waiting 10 s or more: every 10 s, what the work in hand is (kinds,
/// priorities, running or where in line; content-free), to tell why the wait is long
/// (2026-11-22 alpha: "hung on next in line").
fn log_waits(engine: &Engine, st: &mut State) {
    let now = std::time::Instant::now();
    let ten = std::time::Duration::from_secs(10);
    let due: Vec<(String, f64, bool)> = st
        .active
        .iter()
        .filter_map(|(p, a)| {
            let since = a.waiting_since?;
            let long = now.duration_since(since) >= ten;
            let quiet = a.wait_logged.is_none_or(|t| now.duration_since(t) >= ten);
            // Ready already: the wait is the reader's (they move on with their own tap).
            let ready = st.plays.plays.get(p).is_some_and(|pl| {
                let (Some(h), Some(exit)) = (pl.path.last(), a.waiting) else {
                    return false;
                };
                st.graph
                    .follow(&h.beat, exit, None)
                    .is_some_and(|c| crate::model::screen_ready(&st.graph, c))
            });
            (long && quiet).then(|| (p.clone(), now.duration_since(since).as_secs_f64(), ready))
        })
        .collect();
    if due.is_empty() {
        return;
    }
    let backend = &engine.inner.backend;
    let mut work: Vec<(Priority, String)> = st
        .tasks
        .iter()
        .map(|(k, t)| {
            let at = match &t.job {
                None => "unsent".to_string(),
                Some(j) if backend.loading(j) => "loading".into(),
                Some(j) if t.began || backend.started(j) => "running".into(),
                Some(j) => match backend.queued(j) {
                    Some(n) => format!("line{n}"),
                    None => "sent".into(),
                },
            };
            (
                t.priority,
                format!("{}:{:?}@{}:{at}", key_kind(k), t.kind, t.priority),
            )
        })
        .collect();
    work.sort_by_key(|a| std::cmp::Reverse(a.0));
    let work: Vec<String> = work.into_iter().take(8).map(|(_, s)| s).collect();
    let work = work.join(" ");
    for (p, secs, ready) in due {
        if let Some(a) = st.active.get_mut(&p) {
            a.wait_logged = Some(now);
        }
        diag!(
            event = "waiting",
            secs = secs,
            ready = ready,
            work = work.as_str()
        );
    }
}

async fn tick(engine: &Engine) {
    let (start, repri, cancel): (Vec<Want>, Vec<(JobId, Priority)>, Vec<JobId>) = {
        let mut st = engine.st();
        log_waits(engine, &mut st);
        let wants = if st.paused || st.held {
            vec![]
        } else {
            schedule(&st, &engine.inner.cfg)
        };
        let wanted: HashMap<&TaskKey, Priority> =
            wants.iter().map(|w| (&w.key, w.priority)).collect();
        // Running work: follow the new priority, or demote it if no longer wanted.
        let mut repri = Vec::new();
        let keys: Vec<TaskKey> = st.tasks.keys().cloned().collect();
        for k in keys {
            let p = wanted.get(&k).copied().unwrap_or(prio::ORPHAN);
            let t = st.tasks.get_mut(&k).expect("task");
            // Also one the backend doesn't have yet (changed while it was being sent).
            if t.priority != p || t.sent != p {
                t.priority = p;
                if let Some(j) = &t.job {
                    repri.push((j.clone(), p));
                }
            }
        }
        let mut running: HashMap<JobKind, usize> = HashMap::new();
        for t in st.tasks.values() {
            *running.entry(t.kind).or_default() += 1;
        }
        let mut total = st.tasks.len();
        let cfg = &engine.inner.cfg;
        let mut start = Vec::new();
        let mut lined = Vec::new();
        // While a reader's next screens still need work, nothing far off starts on the
        // GPU in their way (a far-off video once took it for 142 s).
        // A next screen still to be made (one failing, waiting to be tried again, holds
        // nothing up meanwhile).
        let now = std::time::Instant::now();
        let now_needed = wants.iter().any(|w| {
            w.priority >= prio::NOW
                && !st
                    .failures
                    .get(&w.key)
                    .is_some_and(|(_, until)| now < *until)
        });
        for w in wants {
            if st.tasks.contains_key(&w.key) {
                continue;
            }
            if now_needed && w.kind != JobKind::Text && w.priority < prio::MEDIA_NOW {
                if lined.len() < 12 {
                    lined.push((w.key.clone(), w.kind, w.priority));
                }
                continue;
            }
            if st
                .failures
                .get(&w.key)
                .is_some_and(|(_, until)| std::time::Instant::now() < *until)
            {
                continue;
            }
            let n = running.entry(w.kind).or_default();
            // Only what a reader waits on skips the line: sending every next screen at once
            // filled the backend's line and made the reader's own work wait (mock tune:
            // worst wait 263 s with that, 69 s without). Next screens still come first,
            // by priority.
            // Over the limit only to interrupt: higher on the ladder than all that's sent.
            let interrupts = st
                .tasks
                .values()
                .all(|t| prio::tier(t.priority) < prio::tier(w.priority));
            if (*n >= cap(cfg, w.kind) || total >= cfg.max_jobs) && !interrupts {
                if lined.len() < 12 {
                    lined.push((w.key.clone(), w.kind, w.priority));
                }
                continue;
            }
            *n += 1;
            total += 1;
            st.tasks.insert(
                w.key.clone(),
                Task {
                    kind: w.kind,
                    priority: w.priority,
                    job: None,
                    started: std::time::Instant::now(),
                    preempted: false,
                    cut_in: false,
                    began: false,
                    sent: w.priority,
                },
            );
            start.push(w);
        }
        st.lined = lined;
        // What's actually on a worker (it stays "being made" across tool rounds).
        let backend = engine.inner.backend.clone();
        for t in st.tasks.values_mut() {
            if !t.began && t.job.as_ref().is_some_and(|j| backend.started(j)) {
                t.began = true;
            }
        }
        let mut cancel = Vec::new();
        // Rule 3: work sent earlier without cutting in, risen since up the ladder (a next
        // screen once far off; a choice since picked) and still waiting its turn behind
        // something lower: sent again to cut in. A queued job loses nothing (2026-11-21
        // alpha: a picked choice sat "2nd in line"). Only with something on a worker that
        // sending it again would cut in over (else it would be sent again and again).
        let running_top = st
            .tasks
            .values()
            .filter(|t| t.began)
            .map(|t| prio::tier(t.sent))
            .max();
        // Only the front of the line (as `run_job` cuts in): the rest follow it in order.
        let line: Vec<(Priority, bool)> = st
            .tasks
            .values()
            .filter(|t| !t.began)
            .map(|t| (t.priority, t.cut_in))
            .collect();
        for t in st.tasks.values_mut() {
            let front = line
                .iter()
                .filter(|(p, c)| *p > t.priority || (*p == t.priority && *c))
                .count()
                == 0;
            if front
                && running_top.is_some_and(|r| prio::tier(t.priority) > r)
                && !t.cut_in
                && !t.began
                && !t.preempted
                && let Some(j) = &t.job
            {
                t.preempted = true;
                cancel.push(j.clone());
            }
        }
        // Rule 2: running text lower on the ladder than work waiting for a worker is
        // cancelled (text can't pause; pictures and video pause by themselves for what cuts
        // in), unless it's about done. Never for work on its own rung (rule 1: siblings
        // take turns). The highest rung with work waiting for a worker:
        let waiting_top = st
            .tasks
            .values()
            .filter(|t| !t.began)
            .map(|t| prio::tier(t.priority))
            .max();
        if let Some(min_left) = cfg.preempt_secs
            && let Some(top) = waiting_top
        {
            let durations = st.durations.clone();
            // Sounds take far longer than a line of speech (thinfer 7857f42: ~54 s).
            let sounds: HashSet<TaskKey> = st
                .tasks
                .keys()
                .filter(|k| {
                    matches!(k, TaskKey::Media { media } if st.graph.media.get(media)
                        .and_then(|m| m.audio.as_ref())
                        .is_some_and(|a| a.role == AudioRole::Sound))
                })
                .cloned()
                .collect();
            for (k, t) in st.tasks.iter_mut() {
                // Text and audio can't pause (a sound runs ~50 s): made again later.
                if !matches!(t.kind, JobKind::Text | JobKind::Audio)
                    || prio::tier(t.priority) >= top
                    || t.preempted
                    || !t.began
                {
                    continue;
                }
                let Some(j) = &t.job else { continue };
                let mut est = durations.get(&t.kind).copied().unwrap_or(8.0);
                if sounds.contains(k) {
                    est = est.max(50.0);
                }
                if est - t.started.elapsed().as_secs_f64() > min_left {
                    t.preempted = true;
                    cancel.push(j.clone());
                }
            }
        }
        (start, repri, cancel)
    };
    for (j, p) in repri {
        engine.inner.backend.reprioritize(&j, p).await;
        // The backend has it now.
        if let Some(t) = engine
            .st()
            .tasks
            .values_mut()
            .find(|t| t.job.as_ref() == Some(&j))
        {
            t.sent = p;
        }
    }
    for j in cancel {
        engine.inner.backend.cancel(&j).await;
    }
    for w in start {
        let e = engine.clone();
        tokio::spawn(async move {
            let key = w.key.clone();
            let t0 = std::time::Instant::now();
            let result = match &w.key {
                TaskKey::Media { media } => run_media(&e, &key, media).await,
                _ => run_gen(&e, &key, w.path).await,
            };
            // What the GPU did, so any wait can be explained (no content).
            diag!(
                event = "job",
                task = key_kind(&key),
                kind = ?w.kind,
                priority = w.priority,
                secs = t0.elapsed().as_secs_f64(),
                outcome = match &result {
                    Ok(()) => "done",
                    Err(m) if m == "cancelled" => "cancelled",
                    Err(_) => "failed",
                },
            );
            // A generation that failed takes its streamed lines back off the screen.
            let withdrawn = {
                let mut st = e.st();
                // Cancelled only to be sent again (to cut in, or out of a reader's way):
                // an edit request keeps its place (else the author's fork was lost).
                let resend = st.tasks.remove(&key).is_some_and(|t| t.preempted)
                    && (st.held || matches!(&result, Err(m) if m == "cancelled"));
                if !resend && !matches!(key, TaskKey::Gen { .. } | TaskKey::Media { .. }) {
                    st.wanted_paths.remove(&key);
                    st.notes.remove(&key);
                }
                match &key {
                    TaskKey::Gen { from, exit, .. } | TaskKey::Fork { from, exit, .. } => {
                        st.written.remove(&(from.clone(), *exit));
                        st.drafts
                            .remove(&(from.clone(), *exit))
                            .map(|_| (from.clone(), *exit))
                    }
                    _ => None,
                }
            };
            if let Some((from, exit)) = withdrawn {
                e.emit(Event::Draft { from, exit });
            }
            match result {
                Ok(()) => {
                    e.st().failures.remove(&key);
                }
                Err(msg) if msg == "cancelled" => {}
                // The GPU was given back under it (its server stopped): made on resume.
                Err(_) if e.st().held => {}
                Err(msg) => {
                    let n = {
                        let mut st = e.st();
                        let f = st
                            .failures
                            .entry(key.clone())
                            .or_insert((0, std::time::Instant::now()));
                        f.0 += 1;
                        f.1 = std::time::Instant::now() + backoff(f.0);
                        f.0
                    };
                    tracing::warn!("generation failed ({n} in a row): {msg}");
                    diag!(
                        event = "failed",
                        task = key_kind(&key),
                        in_a_row = n,
                        class = error_class(&msg),
                    );
                    // Said once, and again if it keeps failing; not on every retry.
                    if n == 1 || n == 4 {
                        let message = if n == 1 {
                            msg
                        } else {
                            format!("still failing, retrying now and then: {msg}")
                        };
                        e.emit(Event::Error { message });
                    }
                }
            }
            e.wake();
        });
    }
}

/// Run one backend job, recording its id so the scheduler can re-prioritize it.
async fn run_job(
    e: &Engine,
    key: &TaskKey,
    job: Job,
) -> std::result::Result<JobOutput, BackendError> {
    run_job_streaming(e, key, job, &mut |_| {}).await
}

/// [`run_job`], handing a text reply's pieces to `on_text` as they're generated.
async fn run_job_streaming(
    e: &Engine,
    key: &TaskKey,
    job: Job,
    on_text: &mut (dyn FnMut(&str) + Send),
) -> std::result::Result<JobOutput, BackendError> {
    let priority = e.st().tasks.get(key).map(|t| t.priority).unwrap_or(0);
    let kind = job.kind();
    let t0 = std::time::Instant::now();
    // What a reader is waiting on runs next (its text, or the picture their next screen
    // needs): a picture or video in the way pauses and resumes after it, losing nothing.
    // A video never cuts in: it's long, and nothing pauses for it.
    // Never for something less urgent than what's running: the backend pauses whatever
    // runs, so a choice written ahead paused the opening's picture a reader was waiting
    // on (2026-11-21 alpha: 84 s for a 25 s picture).
    // Only over something less urgent: a picture paused an equally urgent one (80 s),
    // and when pictures could only cut in for a waiting reader, a far-off video kept the
    // reader's next picture waiting 137 s (2026-11-21 alpha).
    // Only what a reader waits on or picked cuts in (rule 2), and only over something less
    // urgent: next screens among themselves just take turns (2026-11-21 alpha: a picture
    // paused an equally urgent one, 80 s; a video paused again and again, 522 s).
    // "Running" means on a worker, the same as rule 3 in tick sees it: a queued job as
    // urgent doesn't stop this one cutting in (else rule 3 cancelled and sent it again
    // every tick while that one waited its turn).
    // The ladder (`prio::tier`): it cuts in over anything lower that runs, never over
    // the same rung (siblings take turns: no pausing back and forth).
    // Only the front of the line cuts in: something at least as urgent waiting for a worker
    // goes first. Cutting in ahead of it jumped it, and when a worker took it in the
    // moment before this reached the backend, paused it: the same rung, or higher
    // (2026-11-22 test: 950 and even 650 cut in over a 964 picture). Decided and marked
    // under one lock, so two at once can't both cut in.
    let preempt = {
        let backend = e.inner.backend.clone();
        let mine = prio::tier(priority);
        let mut st = e.st();
        let started = |t: &Task| t.began || t.job.as_ref().is_some_and(|j| backend.started(j));
        let running: Vec<Priority> = st
            .tasks
            .iter()
            .filter(|(k, t)| *k != key && started(t))
            .map(|(_, t)| prio::tier(t.sent))
            .collect();
        let ahead = st.tasks.iter().any(|(k, t)| {
            k != key
                && !started(t)
                && (t.priority > priority || (t.priority == priority && t.cut_in))
        });
        let p = !ahead && !running.is_empty() && running.iter().all(|r| *r < mine);
        if let Some(t) = st.tasks.get_mut(key) {
            t.cut_in = p;
        }
        p
    };
    let sub = e.inner.backend.submit(job, priority, preempt).await?;
    if let Some(t) = e.st().tasks.get_mut(key) {
        t.job = Some(sub.id.clone());
        // If it changed while this was on its way, the tick sends the change.
        t.sent = priority;
    }
    let mut done = sub.done;
    let mut text = sub.text;
    let result = loop {
        match text.as_mut() {
            Some(rx) => tokio::select! {
                r = &mut done => break r,
                piece = rx.recv() => match piece {
                    Some(p) => on_text(&p),
                    None => text = None,
                },
            },
            None => break (&mut done).await,
        }
    };
    if let Some(rx) = text.as_mut() {
        while let Ok(p) = rx.try_recv() {
            on_text(&p);
        }
    }
    match result {
        Ok(o) => {
            let secs = t0.elapsed().as_secs_f64();
            let mut st = e.st();
            let d = st.durations.entry(kind).or_insert(secs);
            *d = *d * 0.7 + secs * 0.3;
            Ok(o)
        }
        Err(err) => Err(err),
    }
}

/// The chat a beat was written in, recorded on the beat.
struct ChatMeta {
    turn: String,
    reply: String,
    sys: u64,
    seg_start: bool,
}

/// Beats whose session state the backend likely still keeps (it evicts the least
/// recently used past its budget; a handful of ~100-400 MB states fit).
const WARM: usize = 6;

/// `beat`'s node was just made or used.
fn warm_up(st: &mut State, beat: &BeatId) {
    st.warm.retain(|b| b != beat);
    st.warm.push_back(beat.clone());
    while st.warm.len() > WARM {
        st.warm.pop_front();
    }
}

/// The text session for `model`, opening one if needed. `None`: the backend has none.
async fn session_for(e: &Engine, model: &str) -> Option<String> {
    if !e.inner.cfg.sessions {
        return None;
    }
    if let Some((m, s)) = &e.st().session
        && m == model
    {
        return Some(s.clone());
    }
    let s = e.inner.backend.create_session(model).await?;
    let old = {
        let mut st = e.st();
        st.nodes.clear();
        st.warm.clear();
        st.session.replace((model.to_string(), s.clone()))
    };
    if let Some((_, o)) = old {
        e.inner.backend.drop_session(&o).await;
    }
    Some(s)
}

async fn run_gen(e: &Engine, key: &TaskKey, parent: Path) -> std::result::Result<(), String> {
    let target = match key {
        TaskKey::Rerun { beat } => Some(beat.clone()),
        _ => None,
    };
    let cfg = &e.inner.cfg;
    let (chat, (model, think), used, allow) = {
        let st = e.st();
        let res = Resolver::new(&st.graph, &parent, target.as_deref());
        let mut chat = context::chat(
            &st.graph,
            &parent,
            target.as_deref(),
            &Limits::default(),
            cfg.segment,
        );
        if let Some(n) = st.notes.get(key) {
            chat.user
                .push_str(&format!("Direction for this beat (from the author): {n}\n"));
        }
        (
            chat,
            {
                let s = res.settings();
                (s.text_model, s.think)
            },
            res.used(),
            {
                // Only what the generator can make.
                let s = res.settings();
                let mut a = context::allow(&st.graph, &parent, &Limits::default());
                if !can_run(&st.models, &s.sound_model) {
                    a.sounds = false;
                }
                if !can_run(&st.models, &s.voice_design_model)
                    && !can_run(&st.models, &s.voice_model)
                {
                    a.deliveries = None;
                }
                a
            },
        )
    };
    let full = chat.messages();
    // With a session, continue from the previous beat's node and send only the new turn.
    let mut session = session_for(e, &model).await;
    let resume = |e: &Engine, session: &Option<String>| -> (Option<SessionRef>, Vec<Message>) {
        let Some(s) = session else {
            return (None, full.clone());
        };
        let node = chat
            .history
            .last()
            .and_then(|(b, _, _)| e.st().nodes.get(b).cloned());
        match node {
            Some(n) => (
                Some(SessionRef {
                    session: s.clone(),
                    from: Some(n),
                }),
                vec![Message::new("user", chat.user.clone())],
            ),
            None => (
                Some(SessionRef {
                    session: s.clone(),
                    from: None,
                }),
                full.clone(),
            ),
        }
    };
    let (mut sref, mut send) = resume(e, &session);
    if sref.as_ref().is_some_and(|r| r.from.is_some())
        && let Some((b, _, _)) = chat.history.last()
    {
        // The backend counts it as used too.
        warm_up(&mut e.st(), b);
    }
    // Lines stream to a waiting player as they're written (not for re-runs: the old take
    // is still on screen).
    let draft_key = match key {
        TaskKey::Gen { from, exit, .. } | TaskKey::Fork { from, exit, .. } => {
            Some((from.clone(), *exit))
        }
        _ => None,
    };
    let mut tool_rounds = 0;
    let mut attempts = 0;
    let mut gone_retries = 0;
    let (out, reply, node) = loop {
        // A beat that keeps failing tries without tools (a lookup the backend can't
        // take would otherwise fail it forever; 2026-11-21 alpha).
        let failing = e.st().failures.get(key).is_some_and(|f| f.0 >= 2);
        let tools = if tool_rounds < cfg.tool_rounds && !failing {
            contract::tools()
        } else {
            vec![]
        };
        let req = TextRequest {
            model: model.clone(),
            messages: send.clone(),
            tools,
            schema: Some(contract::schema_with(&allow)),
            max_tokens: Some(1200),
            temperature: Some(0.9),
            seed: None,
            think,
            session: sref.clone(),
        };
        let mut buf = String::new();
        let mut shown = 0;
        let t0 = std::time::Instant::now();
        let mut first_line = None;
        let resumed = sref.as_ref().is_some_and(|r| r.from.is_some());
        let messages = send.len();
        let priority = e.st().tasks.get(key).map(|t| t.priority).unwrap_or(0);
        let mut on_text = |piece: &str| {
            buf.push_str(piece);
            if let Some(k) = &draft_key {
                e.st().written.insert(k.clone(), buf.len());
            }
            let lines = contract::partial_lines(&buf);
            if lines.len() > shown {
                shown = lines.len();
                first_line.get_or_insert_with(|| t0.elapsed().as_secs_f64());
                let Some(k) = &draft_key else { return };
                e.st().drafts.insert(k.clone(), lines);
                e.emit(Event::Draft {
                    from: k.0.clone(),
                    exit: k.1,
                });
            }
        };
        let result = run_job_streaming(e, key, Job::Text(req), &mut on_text).await;
        let mut stat = TextStat {
            wall_secs: t0.elapsed().as_secs_f64(),
            first_line_secs: first_line,
            resumed,
            messages,
            priority,
            ..TextStat::default()
        };
        if let Ok(JobOutput::Text(t)) = &result {
            stat.prefill_secs = t.prefill_secs;
            stat.decode_secs = t.decode_secs;
            stat.prompt_tokens = t.prompt_tokens;
            stat.generated_tokens = t.generated_tokens;
            stat.tool_round = !t.tool_calls.is_empty();
        }
        stat.gone = matches!(result, Err(BackendError::SessionGone));
        if !matches!(result, Err(BackendError::Cancelled)) {
            e.record_text(stat);
        }
        let resp = match result {
            Ok(JobOutput::Text(t)) => t,
            Ok(_) => return Err("backend returned media for a text job".into()),
            Err(BackendError::SessionGone) if gone_retries < 2 => {
                gone_retries += 1;
                let spot = sref.as_ref().is_some_and(|r| r.from.is_some());
                if gone_retries == 1 && spot {
                    // Only the spot it went on from was let go (the backend's budget):
                    // that beat starts over in the same session; every other line keeps
                    // its state (before, the whole session was dropped, so nothing
                    // resumed after that).
                    let mut st = e.st();
                    if let Some((b, _, _)) = chat.history.last() {
                        st.nodes.remove(b);
                        st.warm.retain(|w| w != b);
                    }
                } else {
                    // The session itself is gone: a fresh one.
                    e.st().session = None;
                    session = session_for(e, &model).await;
                }
                (sref, send) = (
                    session.clone().map(|s| SessionRef {
                        session: s,
                        from: None,
                    }),
                    full.clone(),
                );
                tool_rounds = 0;
                continue;
            }
            Err(err) => return Err(err.to_string()),
        };
        // Continue a session from this reply with just the next turns; without one, the
        // whole conversation grows.
        let next =
            |sref: &mut Option<SessionRef>, send: &mut Vec<Message>, turns: Vec<Message>| match (
                sref.as_mut(),
                &resp.session_node,
            ) {
                (Some(r), Some(n)) => {
                    r.from = Some(n.clone());
                    *send = turns;
                }
                _ => {
                    let mut call = Message::new("assistant", resp.content.clone());
                    call.tool_calls = resp.tool_calls.clone();
                    send.push(call);
                    send.extend(turns);
                }
            };
        if !resp.tool_calls.is_empty() && tool_rounds < cfg.tool_rounds {
            tool_rounds += 1;
            let turns: Vec<Message> = {
                let st = e.st();
                resp.tool_calls
                    .iter()
                    .map(|tc| {
                        let result = context::run_tool(&st.graph, &parent, &tc.name, &tc.arguments);
                        let mut m = Message::new("tool", result.to_string());
                        m.tool_call_id = Some(tc.id.clone());
                        m.name = Some(tc.name.clone());
                        m
                    })
                    .collect()
            };
            next(&mut sref, &mut send, turns);
            continue;
        }
        let parsed = contract::parse(&resp.content);
        if parsed.is_err() {
            // The reply's shape, never its words: how long, where it stopped, whether
            // it went round in circles (2026-11-23: replies ran to the token cap).
            let s = contract::shape(&resp.content);
            diag!(
                event = "bad_reply",
                chars = resp.content.len(),
                tokens = resp.generated_tokens,
                closed = s.closed,
                last_key = %s.last_key,
                lines = s.lines,
                repeated = s.repeated,
            );
        }
        match parsed {
            Ok(b) => break (b, resp.content.clone(), resp.session_node.clone()),
            Err(err) if attempts < cfg.retries => {
                attempts += 1;
                let fix = Message::new(
                    "user",
                    format!(
                        "That reply was invalid ({err}). Reply with only the beat JSON object."
                    ),
                );
                next(&mut sref, &mut send, vec![fix]);
            }
            Err(err) => return Err(err),
        }
    };
    // Backends that don't enforce the schema may still offer choices too soon.
    let mut out = out;
    // "Ruth: 'Go on then.'" written as narration: the speaker goes where it belongs (when
    // it's someone the story knows, or who speaks in this beat).
    {
        let st = e.st();
        let known: Vec<String> = Resolver::new(&st.graph, &parent, target.as_deref())
            .objects()
            .iter()
            .map(|o| o.name.clone())
            .chain(out.lines.iter().filter_map(|l| l.speaker.clone()))
            .collect();
        contract::lift_speakers(&mut out.lines, &known);
        contract::tidy_lines(&mut out.lines, &known);
    }
    if !allow.choices && matches!(out.next, NextOut::Choices(_)) {
        out.next = NextOut::Continue;
    }
    if !allow.new_media && matches!(out.media, MediaOut::Image { .. } | MediaOut::Video { .. }) {
        out.media = MediaOut::Keep;
    }
    if !allow.end && matches!(out.next, NextOut::End) {
        out.next = NextOut::Continue;
    }
    if allow.must_show
        && matches!(out.media, MediaOut::Keep)
        && !allow.new_media
        && let Some(m) = allow.reuse.as_ref().and_then(|r| r.first())
    {
        out.media = MediaOut::Reuse(m.clone());
    }
    if allow.must_show && allow.new_media && matches!(out.media, MediaOut::Keep) {
        // A backend that ignored the schema: paint the scene from what the beat says.
        let text: Vec<&str> = out.lines.iter().map(|l| l.text.as_str()).collect();
        out.media = MediaOut::Image {
            prompt: text.join(" ").chars().take(400).collect(),
            caption: out.label.clone().unwrap_or_default(),
            objects: vec![],
            refs: vec![],
        };
    }
    // The shot step: a new picture or video written out in full for its model.
    let mut detailed = false;
    let video_on = {
        let st = e.st();
        Resolver::new(&st.graph, &parent, target.as_deref())
            .settings()
            .video
    };
    // With who says them: a speaker is in the scene (and their look goes to the shot).
    let lines: Vec<String> = out
        .lines
        .iter()
        .map(|l| match &l.speaker {
            Some(sp) => format!("{sp}: {}", l.text),
            None => l.text.clone(),
        })
        .collect();
    let is_video = matches!(out.media, MediaOut::Video { .. });
    let seconds = match &out.media {
        MediaOut::Video { seconds, .. } => seconds
            .filter(|s| s.is_finite())
            .map_or(VIDEO_SECS.1, |s| s.clamp(VIDEO_SECS.0, VIDEO_SECS.1)),
        _ => 0.0,
    };
    match &mut out.media {
        MediaOut::Image {
            prompt,
            objects,
            refs,
            ..
        }
        | MediaOut::Video {
            prompt,
            objects,
            refs,
            ..
        } => {
            let kind = if is_video && video_on {
                MediaKind::Video
            } else {
                MediaKind::Image
            };
            let ctx = ShotCtx {
                model: &model,
                parent: &parent,
                session: session.as_deref(),
                node: node.as_deref(),
                full: &full,
                reply: &reply,
                kind,
                idea: prompt.as_str(),
                objects: objects.as_slice(),
                refs: refs.as_slice(),
                lines: &lines,
                seconds,
            };
            match shot_step(e, key, ctx).await {
                Ok(Some((p, visible))) => {
                    *prompt = p;
                    detailed = true;
                    // Only what's in view gets its setup picture as a reference: someone
                    // the storyteller listed but the shot doesn't show (the player behind
                    // the camera, in a first-person shot) would be painted in from it.
                    let st = e.st();
                    let all = Resolver::new(&st.graph, &parent, target.as_deref()).objects();
                    let seen = |name: &str| visible.iter().any(|w| shot::names_match(w, name));
                    let mut shown: Vec<String> =
                        objects.iter().filter(|n| seen(n)).cloned().collect();
                    for o in &all {
                        if seen(&o.name) && !shown.iter().any(|n| n.eq_ignore_ascii_case(&o.name)) {
                            shown.push(o.name.clone());
                        }
                    }
                    refs.retain(|r| {
                        all.iter()
                            .find(|o| o.refs.iter().any(|x| &x.media == r))
                            .is_none_or(|o| seen(&o.name))
                    });
                    *objects = shown;
                }
                Ok(None) => {}
                Err(c) => return Err(c),
            }
        }
        _ => {}
    }
    let meta = ChatMeta {
        seg_start: chat.seg_start(),
        turn: chat.user.clone(),
        reply,
        sys: chat.sys,
    };
    let reply_len = meta.reply.len() as f64;
    let id =
        commit_beat(e, key, &parent, out, used, meta, node, detailed).map_err(|x| x.to_string());
    if let Some(k) = &draft_key {
        let mut st = e.st();
        st.drafts.remove(k);
        st.written.remove(k);
        st.reply_chars = st.reply_chars * 0.8 + reply_len * 0.2;
    }
    id?;
    Ok(())
}

struct ShotCtx<'a> {
    model: &'a str,
    parent: &'a [Hop],
    session: Option<&'a str>,
    /// The beat's own reply node: the shot turn branches off it (only it is read).
    node: Option<&'a str>,
    full: &'a [Message],
    reply: &'a str,
    kind: MediaKind,
    idea: &'a str,
    objects: &'a [String],
    refs: &'a [String],
    lines: &'a [String],
    /// A video's length (seconds), for its timeline.
    seconds: f32,
}

/// Write a new picture or video out in full (see [`crate::shot`]): one turn, and once
/// more naming what's missing if it came back thin. `None`: keep the beat's own prompt
/// (the step failed; the picture is still made). `Err` only when cancelled.
async fn shot_step(
    e: &Engine,
    key: &TaskKey,
    c: ShotCtx<'_>,
) -> std::result::Result<Option<(String, Vec<String>)>, String> {
    let video = c.kind == MediaKind::Video;
    let turn = {
        let st = e.st();
        shot::turn(
            &st.graph, c.parent, c.kind, c.idea, c.objects, c.refs, c.lines, c.seconds,
        )
    };
    // Who and what the storyteller said is in it (its media objects): each described.
    let named: Vec<String> = c.objects.to_vec();
    let plain = |turns: Vec<Message>| -> Vec<Message> {
        let mut m = c.full.to_vec();
        m.push(Message::new("assistant", c.reply.to_string()));
        m.extend(turns);
        m
    };
    let (mut sref, mut send) = match (c.session, c.node) {
        (Some(s), Some(n)) => (
            Some(SessionRef {
                session: s.to_string(),
                from: Some(n.to_string()),
            }),
            vec![Message::new("user", turn.clone())],
        ),
        _ => (None, plain(vec![Message::new("user", turn.clone())])),
    };
    let t0 = std::time::Instant::now();
    let mut first_gaps = 0usize;
    let mut first_kinds = String::new();
    let (mut prefill, mut decode, mut ptok, mut gtok) = (0.0f64, 0.0f64, 0u32, 0u32);
    let mut best: Option<shot::Shot> = None;
    let mut attempts = 0;
    let mut gone = false;
    let outcome = loop {
        attempts += 1;
        let req = TextRequest {
            model: c.model.to_string(),
            messages: send.clone(),
            tools: vec![],
            schema: Some(shot::schema(video)),
            max_tokens: Some(1100),
            temperature: Some(0.7),
            seed: None,
            think: false,
            session: sref.clone(),
        };
        let resp = match run_job(e, key, Job::Text(req)).await {
            Ok(JobOutput::Text(t)) => t,
            Err(BackendError::Cancelled) => return Err("cancelled".into()),
            // Its node was let go: once more as a whole conversation.
            Err(BackendError::SessionGone) if !gone => {
                gone = true;
                attempts -= 1;
                sref = None;
                send = plain(vec![Message::new("user", turn.clone())]);
                continue;
            }
            Ok(_) | Err(_) => break "failed",
        };
        prefill += resp.prefill_secs;
        decode += resp.decode_secs;
        ptok += resp.prompt_tokens;
        gtok += resp.generated_tokens;
        let parsed = shot::parse(&resp.content);
        let gaps = match &parsed {
            Ok(s) => s.gaps(video, &named),
            Err(err) => vec![err.clone()],
        };
        if attempts == 1 {
            first_gaps = gaps.len();
            first_kinds = shot::gap_kinds(&gaps);
        }
        if let Ok(s) = parsed {
            best = Some(s);
        }
        if gaps.is_empty() {
            break "ok";
        }
        if attempts >= 2 {
            break if best.is_some() { "thin" } else { "failed" };
        }
        let fix = Message::new(
            "user",
            format!(
                "Not enough for the {} model to draw it. Fix these and reply with the whole JSON object again:\n- {}",
                if video { "video" } else { "picture" },
                gaps.join("\n- ")
            ),
        );
        match (sref.as_mut(), &resp.session_node) {
            (Some(r), Some(n)) => {
                r.from = Some(n.clone());
                send = vec![fix];
            }
            _ => {
                sref = None;
                if send.first().is_some_and(|m| m.role == "user") && send.len() == 1 {
                    send = plain(vec![Message::new("user", turn.clone())]);
                }
                send.push(Message::new("assistant", resp.content.clone()));
                send.push(fix);
            }
        }
    };
    let prompt = best.as_ref().map(|s| s.compose(video));
    let last_gaps = best.as_ref().map_or(0, |s| s.gaps(video, &named).len());
    diag!(
        event = "shot",
        kind = if video { "video" } else { "image" },
        outcome,
        attempts,
        secs = t0.elapsed().as_secs_f64(),
        idea_words = c.idea.split_whitespace().count(),
        words = prompt.as_ref().map_or(0, |p| p.split_whitespace().count()),
        subjects = best.as_ref().map_or(0, |s| s.subjects.len()),
        named = named.len(),
        gaps_first = first_gaps,
        gap_kinds = %first_kinds,
        prefill_s = prefill,
        decode_s = decode,
        prompt_tok = ptok,
        gen_tok = gtok,
        gaps_last = last_gaps,
    );
    // With what's in view (the subjects, and the place itself: a station or a ship can
    // be the backdrop), which decides the references. Not "details": the player's
    // hands go there.
    let visible: Vec<String> = best
        .as_ref()
        .map(|s| {
            s.subjects
                .iter()
                // Only a hand or a back in view: no picture of their face is needed.
                .filter(|x| !x.in_view.trim().eq_ignore_ascii_case("part"))
                .map(|x| x.who.clone())
                .chain([s.place.clone()])
                .collect()
        })
        .unwrap_or_default();
    Ok(prompt
        .filter(|p| p.split_whitespace().count() >= 20)
        .map(|p| (p, visible)))
}

/// A video's shortest and longest length (seconds); the longest is also the default.
const VIDEO_SECS: (f32, f32) = (2.0, 5.0);

/// Turn the LLM's beat into records.
#[allow(clippy::too_many_arguments)]
fn commit_beat(
    e: &Engine,
    key: &TaskKey,
    parent: &[Hop],
    out: BeatOut,
    used: Vec<RevId>,
    meta: ChatMeta,
    node: Option<String>,
    detailed: bool,
) -> Result<Option<BeatId>> {
    let now = now_ms();
    let (beat, edges, media) = {
        let st = e.st();
        let g = &st.graph;
        let res = Resolver::new(g, parent, None);
        let settings = res.settings();
        let objects = res.objects();
        let (id, origin, epoch, pending) = match key {
            TaskKey::Gen { from, epoch, exit } if from.is_empty() => {
                if g.root.is_some() {
                    return Ok(None);
                }
                let _ = (epoch, exit);
                // Unseen until a player arrives (it may be written ahead during setup).
                (new_id("b"), None, 0, true)
            }
            // Another version of the opening: one more way the story begins.
            TaskKey::Fork { from, .. } if from.is_empty() => {
                (new_id("b"), Some((String::new(), 0, 0)), 0, false)
            }
            TaskKey::Gen { from, epoch, exit } | TaskKey::Fork { from, epoch, exit } => {
                // The exit may have changed under us (overwritten parent, or filled already).
                let Some(fb) = g.beats.get(from) else {
                    return Ok(None);
                };
                if fb.epoch != *epoch {
                    return Ok(None);
                }
                if matches!(key, TaskKey::Gen { .. }) && !g.versions(from, *exit).is_empty() {
                    return Ok(None);
                }
                let pending = !matches!(key, TaskKey::Fork { .. });
                (new_id("b"), Some((from.clone(), *epoch, *exit)), 0, pending)
            }
            TaskKey::Rerun { beat } => {
                let Some(old) = g.beats.get(beat) else {
                    return Ok(None);
                };
                // An unseen beat stays unseen (the opening rewritten before anyone starts).
                (beat.clone(), old.origin.clone(), old.epoch + 1, old.pending)
            }
            TaskKey::Media { .. } => unreachable!(),
        };

        let mut media: Vec<Media> = vec![];
        let show = match out.media {
            MediaOut::Keep => Show::Keep,
            MediaOut::Reuse(m) => {
                if g.media
                    .get(&m)
                    .is_some_and(|m| m.status != MediaStatus::Failed)
                {
                    Show::Reuse { media: m }
                } else {
                    Show::Keep
                }
            }
            out_media @ (MediaOut::Image { .. } | MediaOut::Video { .. }) => {
                let (kind, prompt, caption, objs, refs, seconds) = match out_media {
                    MediaOut::Image {
                        prompt,
                        caption,
                        objects,
                        refs,
                    } => (MediaKind::Image, prompt, caption, objects, refs, None),
                    MediaOut::Video {
                        prompt,
                        caption,
                        objects,
                        refs,
                        seconds,
                    } => {
                        // Video off for this story: make a picture instead.
                        let k = if settings.video {
                            MediaKind::Video
                        } else {
                            MediaKind::Image
                        };
                        // As long as the moment needs (a video's time grows with its
                        // length: 3 s instead of 5 is ~40% sooner).
                        let secs = seconds
                            .filter(|s| s.is_finite())
                            .map(|s| s.clamp(VIDEO_SECS.0, VIDEO_SECS.1));
                        (k, prompt, caption, objects, refs, secs)
                    }
                    _ => unreachable!(),
                };
                let known: HashSet<&str> = objects.iter().map(|o| o.name.as_str()).collect();
                // The same picture asked for again (same words, same references, same
                // size): shown again, not painted again (2026-11-21 alpha).
                let norm = |t: &str| {
                    t.split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase()
                };
                let want = norm(&prompt);
                // Nearly the same shot of the same ones just shown on this path: shown
                // again too (the storyteller can't see pictures; 2026-11-21 alpha, a
                // near-copy was painted again).
                let words = |t: &str| -> HashSet<String> {
                    t.split(|c: char| !c.is_alphanumeric())
                        .filter(|w| w.len() > 3)
                        .map(str::to_lowercase)
                        .collect()
                };
                let mine = words(&prompt);
                let who = |os: &[String]| -> HashSet<String> {
                    os.iter().map(|o| o.to_lowercase()).collect()
                };
                let my_who = who(&objs);
                let recent: HashSet<&str> = parent
                    .iter()
                    .rev()
                    .take(12)
                    .filter_map(|h| match &g.beats.get(&h.beat)?.show {
                        Show::New { media } | Show::Reuse { media } => Some(media.as_str()),
                        Show::Keep => None,
                    })
                    .collect();
                let close = |m: &Media| {
                    if !recent.contains(m.id.as_str())
                        || my_who.is_empty()
                        || who(&m.objects) != my_who
                    {
                        return false;
                    }
                    let theirs = words(&m.prompt);
                    let both = mine.intersection(&theirs).count() as f64;
                    let any = mine.union(&theirs).count().max(1) as f64;
                    both / any >= 0.7
                };
                if let Some(same) = g.media.values().find(|m| {
                    m.kind == kind
                        && m.status == MediaStatus::Ready
                        && (m.width, m.height) == settings.size_for(kind)
                        && !want.is_empty()
                        && ((m.refs == refs && norm(&m.prompt) == want) || close(m))
                }) {
                    Show::Reuse {
                        media: same.id.clone(),
                    }
                } else {
                    let mid = new_id("m");
                    media.push(Media {
                        id: mid.clone(),
                        kind,
                        status: MediaStatus::Requested,
                        blob: None,
                        mime: None,
                        caption,
                        objects: objs
                            .into_iter()
                            .filter(|o| known.contains(o.as_str()))
                            .collect(),
                        prompt,
                        refs: refs
                            .into_iter()
                            .filter(|r| g.media.contains_key(r))
                            .collect(),
                        width: settings.size_for(kind).0,
                        height: settings.size_for(kind).1,
                        beat: Some(id.clone()),
                        error: None,
                        at: now,
                        detailed,
                        seconds: if kind == MediaKind::Video {
                            seconds
                        } else {
                            None
                        },
                        audio: None,
                    });
                    Show::New { media: mid }
                }
            }
        };
        // Its lines spoken and its sounds made (the screen comes whole with them), unless
        // it's a new video that makes its own sound: that one says its lines and makes
        // its sounds itself.
        let own_sound = media.iter().any(|m| m.kind == MediaKind::Video)
            && makes_sound(&st.models, &settings.video_model)
            && makes_sound(&st.models, &settings.video_ref_model);
        let mut voices: Vec<Option<MediaId>> = vec![];
        let mut sounds: Vec<SoundCue> = vec![];
        if !own_sound
            && settings.voices
            && (can_run(&st.models, &settings.voice_design_model)
                || can_run(&st.models, &settings.voice_model))
        {
            for l in &out.lines {
                let spoken =
                    speaker_of(&objects, &settings, l).and_then(|o| line_audio(g, o, l, &id, now));
                voices.push(spoken.as_ref().map(|m| m.id.clone()));
                media.extend(spoken);
            }
            if voices.iter().all(Option::is_none) {
                voices.clear();
            }
        }
        if !own_sound && settings.sounds && can_run(&st.models, &settings.sound_model) {
            let last = out.lines.len().saturating_sub(1) as u32;
            for s in out.sounds.iter().take(2) {
                let what = s.sound.trim();
                if what.is_empty() {
                    continue;
                }
                // The same sound again (rain is rain): played again, not made again (one
                // still being made too: this screen waits for it like the other).
                let mine = word_set(what);
                let same = g.media.values().find(|m| {
                    m.kind == MediaKind::Audio
                        && m.status != MediaStatus::Failed
                        && m.audio.as_ref().is_some_and(|a| {
                            a.role == AudioRole::Sound
                                && a.looped == s.looped
                                && jaccard(&word_set(&a.words), &mine) >= 0.7
                        })
                });
                let mid = match same {
                    Some(m) => m.id.clone(),
                    None => {
                        let mid = new_id("m");
                        media.push(Media {
                            id: mid.clone(),
                            kind: MediaKind::Audio,
                            status: MediaStatus::Requested,
                            blob: None,
                            mime: None,
                            caption: what.chars().take(60).collect(),
                            objects: vec![],
                            prompt: String::new(),
                            refs: vec![],
                            width: 0,
                            height: 0,
                            beat: Some(id.clone()),
                            error: None,
                            at: now,
                            detailed: false,
                            seconds: Some(if s.looped { LOOP_SECS } else { SOUND_SECS }),
                            audio: Some(AudioMeta {
                                role: AudioRole::Sound,
                                words: what.to_string(),
                                how: String::new(),
                                ref_words: String::new(),
                                looped: s.looped,
                                seed: None,
                            }),
                        });
                        mid
                    }
                };
                sounds.push(SoundCue {
                    media: mid,
                    line: s.line.min(last),
                    looped: s.looped,
                });
            }
        }
        let on_path: HashSet<&str> = parent.iter().map(|h| h.beat.as_str()).collect();
        let next = match out.next {
            NextOut::Rejoin(t)
                if g.beats.contains_key(&t) && t != id && !g.reaches_any(&t, &on_path) =>
            {
                Next::Rejoin { beat: t }
            }
            NextOut::Rejoin(_) => Next::Continue,
            other => other.into_next(),
        };
        let mut edges = Vec::new();
        if let Some((from, ep, exit)) = &origin
            && !matches!(key, TaskKey::Rerun { .. })
        {
            edges.push(Edge {
                from: from.clone(),
                epoch: *ep,
                exit: *exit,
                to: id.clone(),
            });
        }
        if let Next::Rejoin { beat: t } = &next {
            edges.push(Edge {
                from: id.clone(),
                epoch,
                exit: 0,
                to: t.clone(),
            });
        }
        let beat = Beat {
            id: id.clone(),
            origin,
            epoch,
            lines: out.lines,
            show,
            state: out.state,
            summary: out.summary.filter(|s| !s.trim().is_empty()),
            // Models tend to repeat the scene label; only a new scene gets one.
            label: out.label.filter(|l| {
                let prev = parent
                    .iter()
                    .rev()
                    .find_map(|h| g.beats.get(&h.beat).and_then(|b| b.label.as_deref()))
                    .map(|p| p.trim().eq_ignore_ascii_case(l.trim()));
                !l.trim().is_empty() && prev != Some(true)
            }),
            next,
            used,
            pending,
            at: now,
            turn: Some(meta.turn),
            reply: Some(meta.reply),
            sys: meta.sys,
            seg_start: meta.seg_start,
            kept_going: false,
            voices,
            sounds,
        };
        (beat, edges, media)
    };
    let id = beat.id.clone();
    diag!(
        event = "beat",
        beat = %id,
        task = key_kind(key),
        depth = parent.len() + 1,
        lines = beat.lines.len(),
        chars = beat.lines.iter().map(|l| l.text.chars().count()).sum::<usize>(),
        show = match &beat.show {
            Show::Keep => "keep",
            Show::Reuse { .. } => "reuse",
            Show::New { .. } if media.iter().any(|m| m.kind == MediaKind::Video) => "new_video",
            Show::New { .. } => "new_image",
        },
        next = match &beat.next {
            Next::Continue => "continue",
            Next::Choices { .. } => "choices",
            Next::Rejoin { .. } => "rejoin",
            Next::End => "end",
        },
        exits = beat.next.exits(),
        pending = beat.pending,
    );
    for m in media {
        e.write(Record::Media(m))?;
    }
    e.write(Record::Beat(beat))?;
    for ed in edges {
        e.write(Record::Edge(ed))?;
    }
    // A fork of the opening moves the playthroughs on an opening onto the new one.
    if let TaskKey::Fork { from, .. } = key
        && from.is_empty()
    {
        let plays: Vec<(String, Path)> = e
            .st()
            .plays
            .plays
            .values()
            .filter(|p| p.path.len() == 1)
            .map(|p| (p.id.clone(), p.path.clone()))
            .collect();
        for (pid, mut path) in plays {
            path[0].beat = id.clone();
            e.set_path(&pid, path)?;
        }
    }
    // A fork moves the playthroughs standing on the old version onto the new one.
    if let TaskKey::Fork { from, exit, .. } = key
        && !from.is_empty()
    {
        let plays: Vec<(String, Path)> = e
            .st()
            .plays
            .plays
            .values()
            .filter(|p| {
                let n = p.path.len();
                n >= 2 && p.path[n - 2].beat == *from && p.path[n - 2].exit == Some(*exit)
            })
            .map(|p| (p.id.clone(), p.path.clone()))
            .collect();
        for (pid, mut path) in plays {
            path.last_mut().expect("n >= 2").beat = id.clone();
            e.set_path(&pid, path)?;
        }
    }
    // Its session node first: the event wakes the scheduler, which sends what follows
    // right away and continues from this node (2026-11-21 alpha: recorded after, the
    // first beats after it re-sent the whole setup, ~12 s each).
    if let Some(n) = node {
        let mut st = e.st();
        st.nodes.insert(id.clone(), n);
        warm_up(&mut st, &id);
    }
    e.emit(Event::Beat { beat: id.clone() });
    Ok(Some(id))
}

/// How long a sound effect is, and a background loop (seconds).
const SOUND_SECS: f32 = 6.0;
const LOOP_SECS: f32 = 20.0;

/// Can the generator run this model? Yes when it hasn't said what it runs.
fn can_run(models: &[ModelCaps], id: &str) -> bool {
    models.is_empty() || models.iter().any(|m| m.id == id)
}

/// Does this video model make a soundtrack? Its own word when it's said; else by name
/// (the H3 and LTX families do).
fn makes_sound(models: &[ModelCaps], id: &str) -> bool {
    match models.iter().find(|m| m.id == id) {
        Some(m) => m.makes_audio,
        None => id.starts_with("fasth3") || id.starts_with("ltx"),
    }
}

/// A voice's description and a delivery as one instruction for the speech model.
fn voice_instruct(description: &str, how: &str) -> String {
    let d = description.trim().trim_end_matches('.');
    let h = how.trim();
    match (d.is_empty(), h.is_empty()) {
        (false, false) => format!("{d}; {h}."),
        (false, true) => format!("{d}."),
        (true, false) => format!("{h}."),
        (true, true) => String::new(),
    }
}

fn word_set(t: &str) -> HashSet<String> {
    t.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_lowercase)
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let both = a.intersection(b).count() as f64;
    let any = a.union(b).count().max(1) as f64;
    both / any
}

/// The talking object who says a line: its speaker by name (a first name will do), or
/// for narration the narrator, if the story has one. None: nobody to voice it.
fn speaker_of<'a>(
    objects: &[&'a ObjectRev],
    settings: &Settings,
    l: &Line,
) -> Option<&'a ObjectRev> {
    let o = match &l.speaker {
        None => objects
            .iter()
            .find(|o| settings.narrator.as_deref() == Some(o.id.as_str()))?,
        Some(name) => {
            let n = name.trim().to_lowercase();
            if n.is_empty() {
                return None;
            }
            objects
                .iter()
                .filter(|o| settings.narrator.as_deref() != Some(o.id.as_str()))
                .find(|o| {
                    let k = o.name.trim().to_lowercase();
                    !k.is_empty() && (k == n || k.split_whitespace().next() == Some(n.as_str()))
                })?
        }
    };
    o.talks.then_some(*o)
}

/// A line spoken by `o`: in its delivery's clip's voice when that's made, else from how
/// the voice (or, with no voice written, the character) is described. None: nothing to
/// speak, or nothing to speak it with.
fn line_audio(g: &Graph, o: &ObjectRev, l: &Line, beat: &str, now: u64) -> Option<Media> {
    if !l.text.chars().any(char::is_alphanumeric) {
        return None;
    }
    let v = o.voice.clone().unwrap_or_default();
    let d = v.delivery(l.tone.as_deref());
    let clip = d.and_then(|d| d.clip.as_ref()).filter(|c| {
        g.media
            .get(*c)
            .is_some_and(|m| m.status == MediaStatus::Ready && m.blob.is_some())
    });
    let described = match v.description.trim() {
        "" => o.description.trim(),
        d => d,
    };
    if clip.is_none() && described.is_empty() {
        return None;
    }
    let how = d
        .map(|d| d.how.as_str())
        .filter(|h| !h.trim().is_empty())
        .or(l.tone.as_deref())
        .unwrap_or("");
    Some(Media {
        id: new_id("m"),
        kind: MediaKind::Audio,
        status: MediaStatus::Requested,
        blob: None,
        mime: None,
        caption: o.name.clone(),
        objects: vec![o.name.clone()],
        prompt: String::new(),
        refs: clip.cloned().into_iter().collect(),
        width: 0,
        height: 0,
        beat: Some(beat.to_string()),
        error: None,
        at: now,
        detailed: false,
        seconds: None,
        audio: Some(AudioMeta {
            role: AudioRole::Line,
            words: l.text.clone(),
            how: voice_instruct(described, how),
            ref_words: d.map(|d| d.words.clone()).unwrap_or_default(),
            looped: false,
            // Spoken from a description: the same seed each line keeps it closer to one
            // voice.
            seed: Some(context::hash_text(&o.id)),
        }),
    })
}

/// The references a media prompt uses, in order, and the prompt with each one named the
/// way the model reads it. The LLM may name a picture as `<Picture N>` (the Nth of its
/// "refs"), or by id (`<m1a2...>` or bare): every painted image it names is used, then
/// the rest of its "refs"; with none at all, each named object's first reference
/// picture. A model that takes no references gets the picture's name in words instead.
/// What a task is for, by ids (diagnostics).
fn what(g: &Graph, k: &TaskKey) -> String {
    match k {
        TaskKey::Gen { from, .. } if from.is_empty() => "opening beat".to_string(),
        TaskKey::Gen { from, exit, .. } => format!("beat after {from} exit {exit}"),
        TaskKey::Fork { from, exit, .. } => format!("fork after {from} exit {exit}"),
        TaskKey::Rerun { beat } => format!("rerun {beat}"),
        TaskKey::Media { media } => match g.media.get(media) {
            Some(m) => format!(
                "{} {media} for {}",
                match m.kind {
                    MediaKind::Video => "video",
                    MediaKind::Image => "picture",
                    MediaKind::Audio => "audio",
                },
                m.beat.as_deref().unwrap_or("setup")
            ),
            None => format!("media {media}"),
        },
    }
}

/// How many screens past the reader's `beat` is (0: it is the reader's), going back
/// from `beat` through the beats it came from. None: not ahead of the reader.
fn screens_ahead(g: &Graph, reader: &BeatId, beat: &BeatId) -> Option<usize> {
    let mut cur = beat.clone();
    for n in 0..10_000 {
        if &cur == reader {
            return Some(n);
        }
        cur = g.beats.get(&cur)?.origin.as_ref()?.0.clone();
    }
    None
}

/// "the next screen", "a screen 3 ahead", ...: a screen `n` past the reader's.
fn screen_words(n: Option<usize>) -> String {
    match n {
        Some(0) => "this screen".into(),
        Some(1) => "the next screen".into(),
        Some(n) => format!("a screen {n} ahead"),
        None => "a screen elsewhere in the story".into(),
    }
}

/// What a task is for, in the reader's terms, from where they are.
fn label_for(g: &Graph, k: &TaskKey, reader: Option<&BeatId>) -> String {
    let ahead = |b: &BeatId| reader.and_then(|r| screens_ahead(g, r, b));
    // The screen behind an exit of `from`: one past `from`.
    let after = |from: &BeatId, exit: usize| {
        let n = ahead(from).map(|n| n + 1);
        let choices = g.beats.get(from).map_or(1, |b| b.next.exits());
        let mut s = screen_words(n);
        if choices > 1 {
            s = format!("{s} (choice {})", exit + 1);
        }
        s
    };
    match k {
        TaskKey::Gen { from, .. } if from.is_empty() => "The opening screen".into(),
        TaskKey::Gen { from, exit, .. } => format!("Writing {}", after(from, *exit)),
        TaskKey::Fork { from, .. } if from.is_empty() => "A new version of the opening".into(),
        TaskKey::Fork { from, exit, .. } => {
            format!("A new version of {}", after(from, *exit))
        }
        TaskKey::Rerun { beat } => format!("Rewriting {}", screen_words(ahead(beat))),
        TaskKey::Media { media } => match g.media.get(media) {
            Some(m) if m.kind == MediaKind::Audio => {
                let role = m.audio.as_ref().map_or(AudioRole::Sound, |a| a.role);
                match (role, &m.beat) {
                    (AudioRole::Voice, _) | (_, None) => {
                        format!("Making a voice for {}", m.caption)
                    }
                    (AudioRole::Line, Some(b)) => format!("Voicing {}", screen_words(ahead(b))),
                    (AudioRole::Sound, Some(b)) => {
                        format!("Making a sound for {}", screen_words(ahead(b)))
                    }
                }
            }
            Some(m) => {
                let what = if m.kind == MediaKind::Video {
                    "Filming"
                } else {
                    "Painting"
                };
                match &m.beat {
                    Some(b) => format!("{what} {}", screen_words(ahead(b))),
                    None => format!("{what} a setup picture"),
                }
            }
            None => "A picture".into(),
        },
    }
}

fn resolve_refs(
    g: &Graph,
    m: &Media,
    objects: &[ObjectRev],
    max: usize,
    tag: Option<&str>,
    recent: &[MediaId],
) -> (Vec<MediaId>, String) {
    let usable = |id: &str| {
        g.media
            .get(id)
            .is_some_and(|r| r.status == MediaStatus::Ready && r.kind == MediaKind::Image)
    };
    // What each mention points at, in order of appearance.
    let mut spans: Vec<(usize, usize, MediaId)> = vec![];
    let text = &m.prompt;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let tag = rest
            .strip_prefix("<Picture ")
            .and_then(|t| {
                let d: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
                let n: usize = d.parse().ok()?;
                t[d.len()..]
                    .starts_with('>')
                    .then(|| (n, "<Picture ".len() + d.len() + 1))
            })
            // Past its refs: with only one, that one was meant (a storyteller wrote
            // "starts exactly as <Picture 6>" with the picture on screen its only ref;
            // 2026-11-23); else said in words. Never left as a bare tag for the model.
            .map(|(n, len)| {
                let id = m.refs.get(n.wrapping_sub(1)).or(if m.refs.len() == 1 {
                    m.refs.first()
                } else {
                    None
                });
                (id.cloned().unwrap_or_default(), len)
            });
        let id = || {
            // `<id>` or a bare id at a word boundary.
            let open = rest.starts_with('<');
            let body = if open { &rest[1..] } else { rest };
            let word: String = body
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let before_ok = i == 0 || !(bytes[i - 1] as char).is_ascii_alphanumeric();
            if !before_ok || !g.media.contains_key(&word) {
                return None;
            }
            let close = open && body[word.len()..].starts_with('>');
            if open && !close {
                return None;
            }
            Some((word.clone(), word.len() + if open { 2 } else { 0 }))
        };
        if let Some((id, len)) = tag.or_else(id) {
            spans.push((i, len, id));
            i += len;
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
        }
    }
    let mut refs: Vec<MediaId> = vec![];
    for id in spans.iter().map(|s| &s.2).chain(m.refs.iter()) {
        if usable(id) && !refs.contains(id) {
            refs.push(id.clone());
        }
    }
    // Nothing named by the storyteller: for each one in the shot, the author's own setup
    // picture (and what they wrote it's for). Not a picture made since: that drifted, a
    // copy of a copy, and dropped the author's "use for" (2026-11-22 alpha; the user:
    // always favor the original reference).
    // A video with nothing named starts from the picture on screen, so it carries on
    // the scene instead of making up another (2026-11-21 alpha).
    let mut starts_from = None;
    if refs.is_empty()
        && m.kind == MediaKind::Video
        && let Some(on) = recent.iter().find(|id| {
            **id != m.id
                && usable(id)
                && g.media.get(*id).is_some_and(|x| x.kind == MediaKind::Image)
        })
    {
        refs.push(on.clone());
        starts_from = Some(on.clone());
    }
    // With the shot step (it said who's in view), every one in view gets their setup
    // picture unless one of theirs is already there: a storyteller naming only the
    // picture on screen left a character on a monitor looking like someone else
    // (2026-11-23). Without it, only when nothing was named (as before).
    if m.detailed || refs.len() <= usize::from(starts_from.is_some()) {
        for o in objects {
            if !m.objects.iter().any(|n| n.eq_ignore_ascii_case(&o.name)) {
                continue;
            }
            if o.refs.iter().any(|r| refs.contains(&r.media)) {
                continue;
            }
            if let Some(r) = o.refs.iter().find(|r| usable(&r.media)) {
                refs.push(r.media.clone());
            }
        }
    }
    refs.truncate(max);
    // How the model names its k-th reference; past what it takes, a name in words.
    let word_for = |id: &str| {
        objects
            .iter()
            .find(|o| o.refs.iter().any(|r| r.media == id))
            .map(|o| o.name.clone())
            .or_else(|| g.media.get(id).map(|r| r.caption.trim().to_string()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "an earlier picture".into())
    };
    let label = |id: &str| match (refs.iter().position(|r| r == id), tag) {
        (Some(k), Some(t)) => t.replace("{n}", &(k + 1).to_string()),
        _ => word_for(id),
    };
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (start, len, id) in &spans {
        out.push_str(&text[at..*start]);
        out.push_str(&label(id));
        at = start + len;
    }
    out.push_str(&text[at..]);
    if let Some(on) = &starts_from {
        out = format!(
            "Starts exactly as {}, then: {}",
            label(on),
            out.trim_start()
        );
    }
    // What the author wrote each reference is for, as written, first: picture models
    // weigh the start of a prompt most and follow a reference's look over later words
    // (2026-11-21 alpha).
    // A setup picture with no "use for": what that one looks like, and nothing more (left
    // unsaid, the picture model copied the whole reference, pose and all: a keeper
    // standing as in her portrait where the story had her sit at her desk; 2026-11-23).
    let uses: Vec<String> = refs
        .iter()
        .filter_map(|id| {
            let (o, r) = objects
                .iter()
                .find_map(|o| o.refs.iter().find(|r| &r.media == id).map(|r| (o, r)))?;
            let u = r.use_for.trim();
            Some(if u.is_empty() {
                format!(
                    "{}: what {} looks like, not where it is; the pose, place, light and framing as described here.",
                    label(id),
                    o.name
                )
            } else {
                format!("{}: {u}.", label(id))
            })
        })
        .collect();
    if !uses.is_empty() {
        out = format!("{}\n\n{}", uses.join(" "), out.trim_start());
    }
    (refs, out)
}

/// How a model's prompt names its n-th reference, for models the backend doesn't
/// describe (it says itself through its model list).
fn default_tag(model: &str) -> Option<String> {
    match model {
        "qwen-image-2.1-turbo" => Some("<image{n}>".into()),
        "fasth3-omni" => Some("<Picture {n}>".into()),
        _ => None,
    }
}

/// Reference images a model takes (0: it can't use any), for models the backend
/// doesn't describe.
pub fn refs_accepted(model: &str) -> usize {
    match model {
        "qwen-image-2.1-turbo" => 10,
        "qwen-image-edit-rapid" => 1,
        "fasth3-omni" => 9,
        _ => 0,
    }
}

/// The job that makes an audio clip: a voice's clip (from its description), a line (in
/// its voice's clip, else from the description), or a sound.
fn audio_job(e: &Engine, m: &Media) -> std::result::Result<Job, String> {
    let (req, clip) = {
        let st = e.st();
        let g = &st.graph;
        let at: Path = m
            .beat
            .as_deref()
            .and_then(|b| find_path(g, b))
            .unwrap_or_default();
        let s = Resolver::new(g, &at, None).settings();
        let a = m
            .audio
            .clone()
            .ok_or("an audio clip with nothing to make")?;
        let design = |how: &str| AudioRequest {
            model: s.voice_design_model.clone(),
            text: a.words.clone(),
            instruct: Some(how.to_string()).filter(|h| !h.trim().is_empty()),
            voice: None,
            voice_words: None,
            seconds: None,
            looped: false,
            seed: a.seed,
        };
        match a.role {
            AudioRole::Sound => (
                AudioRequest {
                    model: s.sound_model.clone(),
                    text: a.words.clone(),
                    instruct: None,
                    voice: None,
                    voice_words: None,
                    seconds: m.seconds,
                    looped: a.looped,
                    seed: a.seed,
                },
                None,
            ),
            AudioRole::Voice => (design(&a.how), None),
            AudioRole::Line => {
                let clip = m
                    .refs
                    .first()
                    .and_then(|r| g.media.get(r))
                    .filter(|r| r.status == MediaStatus::Ready)
                    .and_then(|r| r.blob.clone())
                    .filter(|_| can_run(&st.models, &s.voice_model));
                match clip {
                    Some(b) => (
                        AudioRequest {
                            model: s.voice_model.clone(),
                            text: a.words.clone(),
                            instruct: None,
                            voice: None,
                            voice_words: Some(a.ref_words.clone()).filter(|w| !w.trim().is_empty()),
                            seconds: None,
                            looped: false,
                            seed: None,
                        },
                        Some(b),
                    ),
                    None => (design(&a.how), None),
                }
            }
        }
    };
    let mut req = req;
    if let Some(b) = clip {
        req.voice = Some(
            e.inner
                .story
                .read_blob(&b)
                .map_err(|x| x.to_string())?
                .to_vec(),
        );
    }
    Ok(Job::Audio(req))
}

async fn run_media(e: &Engine, key: &TaskKey, media: &str) -> std::result::Result<(), String> {
    // Audio: made from its own particulars, none of a picture's.
    let audio = {
        let st = e.st();
        st.graph
            .media
            .get(media)
            .filter(|m| m.kind == MediaKind::Audio && m.status == MediaStatus::Requested)
            .cloned()
    };
    if let Some(m) = audio {
        let job = audio_job(e, &m)?;
        return finish_media(e, key, m, job, 0).await;
    }
    let (m, prompt, model, refs, loras, voices) = {
        let st = e.st();
        let g = &st.graph;
        let Some(m) = g.media.get(media).cloned() else {
            return Ok(());
        };
        if m.status != MediaStatus::Requested {
            return Ok(());
        }
        // The setup as it stands at the picture's beat: a character or style changed
        // "from here" applies to pictures from there on (resolving at the story's start
        // only saw changes made everywhere; 2026-11-21 alpha).
        let at: Path = m
            .beat
            .as_deref()
            .and_then(|b| find_path(g, b))
            .unwrap_or_default();
        let res = Resolver::new(g, &at, None);
        let settings = res.settings();
        let video = m.kind == MediaKind::Video;
        // The model that will run: the reference-taking one when there are references.
        let (plain, with_refs) = if video {
            (&settings.video_model, &settings.video_ref_model)
        } else {
            (&settings.image_model, &settings.image_edit_model)
        };
        let objects: Vec<ObjectRev> = res.objects().into_iter().cloned().collect();
        let cap = st.models.iter().find(|c| &c.id == with_refs);
        let (max, tag) = match cap {
            Some(c) => (c.max_refs as usize, c.ref_tag.clone()),
            None => (refs_accepted(with_refs), default_tag(with_refs)),
        };
        // Pictures on screen along the way, newest first (for continuity).
        let recent: Vec<MediaId> = at
            .iter()
            .rev()
            .filter_map(|h| match &g.beats.get(&h.beat)?.show {
                Show::New { media } | Show::Reuse { media } => Some(media.clone()),
                Show::Keep => None,
            })
            .collect();
        let (refs, mut prompt) = resolve_refs(g, &m, &objects, max, tag.as_deref(), &recent);
        let model = if refs.is_empty() { plain } else { with_refs }.clone();
        // Who and what is in it, from the author's descriptions: the LLM's prompt often
        // just names them ("Pip offers the seed"), which a picture model can't draw.
        let lower = prompt.to_lowercase();
        let named: Vec<String> = objects
            .iter()
            .filter(|o| {
                m.objects.iter().any(|n| n.eq_ignore_ascii_case(&o.name))
                    || lower.contains(&o.name.to_lowercase())
            })
            .filter(|o| !o.description.trim().is_empty())
            .map(|o| format!("{}: {}", o.name, o.description.trim()))
            .collect();
        if !named.is_empty() && !m.detailed {
            prompt = format!("{}\n\nIn it: {}", prompt.trim_end(), named.join(" "));
        }
        // How they are right now, from the storyteller's notes (clothes and the like
        // change in the story; a picture model only knows what it's told).
        let in_shot: Vec<String> = objects
            .iter()
            .filter(|o| {
                m.objects.iter().any(|n| n.eq_ignore_ascii_case(&o.name))
                    || lower.contains(&o.name.to_lowercase())
            })
            .map(|o| o.name.to_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        let notes: Vec<String> = state_at(g, &at)
            .into_iter()
            .filter(|(k, v)| {
                let (k, v) = (k.to_lowercase(), v.to_lowercase());
                in_shot.iter().any(|n| k.contains(n) || v.contains(n))
            })
            .take(8)
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        if !notes.is_empty() && !m.detailed {
            prompt = format!("{}\n\nNow: {}", prompt.trim_end(), notes.join("; "));
        }
        // The story's visual style goes on every picture (the LLM's prompt describes the
        // shot; left to it, the style was usually dropped and pictures came out photographic).
        if let Some(st) = res
            .prompt(STYLE_PROMPT)
            .map(|p| p.text.trim().to_string())
            .filter(|t| !t.is_empty())
            && !prompt.to_lowercase().contains(&st.to_lowercase())
        {
            prompt = format!("{}\n\nStyle: {st}", prompt.trim_end());
        }
        // The author's picture direction from play, word for word (2026-11-21 alpha: told
        // only to the storyteller, "look at the viewer" never reached the pictures).
        if let Some(d) = res
            .prompts()
            .into_iter()
            .filter(|p| p.id == PICTURE_PROMPT)
            .max_by_key(|p| p.at)
            .map(|p| p.text.trim().to_string())
            .filter(|t| !t.is_empty())
        {
            // First, like a reference's use (see `resolve_refs`).
            prompt = format!("{d}\n\n{}", prompt.trim_start());
        }
        let blobs: Vec<String> = refs
            .iter()
            .filter_map(|r| g.media.get(r).and_then(|r| r.blob.clone()))
            .collect();
        // A video that takes voices: the voice of each one who speaks in it (in view, a
        // line of its beat theirs, a clip of their voice made), and the prompt says whose
        // each is. Only alongside reference pictures (the model's rule), and only the
        // ones who speak (each is more for it to take in).
        let mut voices: Vec<String> = vec![];
        if video && settings.voices && !refs.is_empty() {
            let (amax, atag) = match st.models.iter().find(|c| c.id == model) {
                Some(c) => (c.max_ref_audios as usize, c.audio_tag.clone()),
                None if model == "fasth3-omni" => (3, Some("<Audio {n}>".into())),
                None => (0, None),
            };
            let tag = atag.unwrap_or_else(|| "<Audio {n}>".into());
            let objs: Vec<&ObjectRev> = objects.iter().collect();
            let mut names: Vec<String> = vec![];
            if let Some(b) = m.beat.as_ref().and_then(|b| g.beats.get(b)) {
                for l in b.lines.iter().filter(|l| l.speaker.is_some()) {
                    if names.len() >= amax {
                        break;
                    }
                    let Some(o) = speaker_of(&objs, &settings, l) else {
                        continue;
                    };
                    let in_view = m.objects.is_empty()
                        || m.objects.iter().any(|n| n.eq_ignore_ascii_case(&o.name));
                    if !in_view || names.contains(&o.name) {
                        continue;
                    }
                    let clip = o
                        .voice
                        .as_ref()
                        .and_then(|v| v.delivery(l.tone.as_deref()))
                        .and_then(|d| d.clip.as_ref())
                        .and_then(|c| g.media.get(c))
                        .filter(|c| c.status == MediaStatus::Ready)
                        .and_then(|c| c.blob.clone());
                    if let Some(c) = clip {
                        names.push(o.name.clone());
                        voices.push(c);
                    }
                }
            }
            if !names.is_empty() {
                let said: Vec<String> = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        format!(
                            "{} is the voice-timbre reference for {n}.",
                            tag.replace("{n}", &(i + 1).to_string())
                        )
                    })
                    .collect();
                prompt = format!("{}\n\n{}", prompt.trim_end(), said.join(" "));
            }
        }
        // The story's adapters for the model that runs (picked per model in Settings).
        let loras = settings.loras.get(&model).cloned().unwrap_or_default();
        (m, prompt, model, blobs, loras, voices)
    };
    let mut voice_bytes = Vec::new();
    for b in &voices {
        voice_bytes.push(
            e.inner
                .story
                .read_blob(b)
                .map_err(|x| x.to_string())?
                .to_vec(),
        );
    }
    let mut ref_bytes = Vec::new();
    for b in &refs {
        ref_bytes.push(
            e.inner
                .story
                .read_blob(b)
                .map_err(|x| x.to_string())?
                .to_vec(),
        );
    }
    let job = match m.kind {
        MediaKind::Video => Job::Video(VideoRequest {
            model,
            prompt,
            width: m.width,
            height: m.height,
            seconds: m.seconds.unwrap_or(VIDEO_SECS.1),
            refs: ref_bytes,
            audio: true,
            ref_audios: voice_bytes,
            seed: None,
            loras,
        }),
        _ => Job::Image(ImageRequest {
            model,
            prompt,
            width: m.width,
            height: m.height,
            refs: ref_bytes,
            seed: None,
            loras,
        }),
    };
    finish_media(e, key, m, job, refs.len()).await
}

/// Run a media job and record what came of it.
async fn finish_media(
    e: &Engine,
    key: &TaskKey,
    m: Media,
    job: Job,
    refs: usize,
) -> std::result::Result<(), String> {
    let done = match run_job(e, key, job).await.map_err(|x| x.to_string()) {
        Ok(JobOutput::Media { bytes, mime }) => {
            let blob = e.inner.story.put_blob(&bytes).map_err(|x| x.to_string())?;
            Media {
                status: MediaStatus::Ready,
                blob: Some(blob),
                mime: Some(mime),
                ..m
            }
        }
        Ok(JobOutput::Text(_)) => Media {
            status: MediaStatus::Failed,
            error: Some("backend returned text".into()),
            ..m
        },
        Err(err) if err == "cancelled" => return Ok(()),
        // The GPU was given back under it: still wanted, made on resume.
        Err(_) if e.st().held => return Ok(()),
        Err(err) => {
            // Tried again later (with backoff) before it's given up on.
            let tries = e.st().failures.get(key).map_or(0, |f| f.0) + 1;
            if tries < MEDIA_ATTEMPTS {
                return Err(err);
            }
            Media {
                status: MediaStatus::Failed,
                error: Some(err),
                ..m
            }
        }
    };
    let id = done.id.clone();
    // Asked for again at a new size while this was being made (a new shape in setup):
    // this one is the old size, so it's dropped and the new one made.
    let redone = e
        .st()
        .graph
        .media
        .get(&id)
        .is_some_and(|now| (now.width, now.height) != (done.width, done.height));
    if redone {
        return Ok(());
    }
    diag!(
        event = "media",
        media = %id,
        kind = match done.kind {
            MediaKind::Video => "video",
            MediaKind::Image => "image",
            MediaKind::Audio => "audio",
        },
        status = if done.status == MediaStatus::Ready { "ready" } else { "failed" },
        error = done.error.as_deref().map_or("none", error_class),
        refs = refs,
        width = done.width,
        height = done.height,
        // How much the model was told (a count only), and whether the shot step wrote it.
        words = done.prompt.split_whitespace().count(),
        detailed = done.detailed,
    );
    e.write(Record::Media(done)).map_err(|x| x.to_string())?;
    e.emit(Event::Media { media: id });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pic(id: &str, caption: &str) -> Media {
        Media {
            id: id.into(),
            kind: MediaKind::Image,
            status: MediaStatus::Ready,
            blob: Some("b".into()),
            mime: None,
            caption: caption.into(),
            objects: vec![],
            prompt: String::new(),
            refs: vec![],
            width: 0,
            height: 0,
            beat: None,
            error: None,
            at: 0,
            detailed: false,
            seconds: None,
            audio: None,
        }
    }

    fn beat(id: &str, origin: Option<&str>, next: Next) -> Beat {
        Beat {
            id: id.into(),
            origin: origin.map(|b| (b.into(), 0, 0)),
            epoch: 0,
            lines: vec![],
            show: Show::Keep,
            state: StateOps::default(),
            summary: None,
            label: None,
            next,
            used: vec![],
            pending: false,
            at: 0,
            turn: None,
            reply: None,
            sys: 0,
            seg_start: false,
            kept_going: false,
            voices: vec![],
            sounds: vec![],
        }
    }

    #[test]
    fn the_queue_says_what_work_is_for_from_where_the_reader_is() {
        let mut g = Graph::default();
        let choice = || Choice {
            label: String::new(),
            likely: None,
            own: false,
        };
        let two = Next::Choices {
            choices: vec![choice(), choice()],
        };
        g.beats
            .insert("b1".into(), beat("b1", None, Next::Continue));
        g.beats.insert("b2".into(), beat("b2", Some("b1"), two));
        g.beats
            .insert("b3".into(), beat("b3", Some("b2"), Next::Continue));
        let mut p = pic("m1", "");
        p.beat = Some("b2".into());
        g.media.insert("m1".into(), p);
        let mut v = pic("m2", "");
        v.kind = MediaKind::Video;
        g.media.insert("m2".into(), v);
        let me: BeatId = "b2".into();
        let write = |from: &str, exit| TaskKey::Gen {
            from: from.into(),
            epoch: 0,
            exit,
        };
        let l = |k: &TaskKey| label_for(&g, k, Some(&me));
        assert_eq!(l(&write("b2", 1)), "Writing the next screen (choice 2)");
        assert_eq!(l(&write("b3", 0)), "Writing a screen 2 ahead");
        assert_eq!(
            l(&write("b1", 0)),
            "Writing a screen elsewhere in the story"
        );
        assert_eq!(l(&write("", 0)), "The opening screen");
        assert_eq!(
            l(&TaskKey::Rerun { beat: "b2".into() }),
            "Rewriting this screen"
        );
        assert_eq!(
            l(&TaskKey::Media { media: "m1".into() }),
            "Painting this screen"
        );
        assert_eq!(
            l(&TaskKey::Media { media: "m2".into() }),
            "Filming a setup picture"
        );
    }

    #[test]
    fn a_picture_named_past_the_refs_is_the_only_one_or_words() {
        let mut g = Graph::default();
        for (id, c) in [("m1aa", "on screen"), ("m2bb", "the toad")] {
            g.media.insert(id.into(), pic(id, c));
        }
        let mut v = pic("mv", "x");
        v.kind = MediaKind::Video;
        v.refs = vec!["m1aa".into()];
        v.prompt = "Starts exactly as <Picture 6>, then".into();
        let (refs, p) = resolve_refs(&g, &v, &[], 9, Some("<Picture {n}>"), &[]);
        assert_eq!(refs, ["m1aa"]);
        assert_eq!(p, "Starts exactly as <Picture 1>, then");
        v.refs = vec!["m1aa".into(), "m2bb".into()];
        let (_, p) = resolve_refs(&g, &v, &[], 9, Some("<Picture {n}>"), &[]);
        assert_eq!(p, "Starts exactly as an earlier picture, then");
    }

    #[test]
    fn a_written_out_shot_gets_the_setup_picture_of_each_one_in_view() {
        let mut g = Graph::default();
        for (id, c) in [("m1aa", "Pip reference"), ("mscreen", "on screen")] {
            g.media.insert(id.into(), pic(id, c));
        }
        let pip = ObjectRev {
            id: "o1".into(),
            rev: "r1".into(),
            anchor: Default::default(),
            name: "Pip".into(),
            description: "A robot.".into(),
            refs: vec![RefImage {
                media: "m1aa".into(),
                caption: "Pip".into(),
                use_for: String::new(),
            }],
            talks: false,
            voice: None,
            removed: false,
            at: 0,
        };
        // The storyteller named only the picture on screen; Pip is in view.
        let mut m = pic("mnew", "x");
        m.objects = vec!["Pip".into()];
        m.refs = vec!["mscreen".into()];
        m.prompt = "Pip on <Picture 1>".into();
        m.detailed = true;
        let (refs, _) = resolve_refs(&g, &m, std::slice::from_ref(&pip), 10, None, &[]);
        assert_eq!(refs, ["mscreen", "m1aa"]);
        // Without the shot step, only what was named (as before).
        m.detailed = false;
        let (refs, _) = resolve_refs(&g, &m, std::slice::from_ref(&pip), 10, None, &[]);
        assert_eq!(refs, ["mscreen"]);
    }

    #[test]
    fn references_follow_the_llms_naming_whichever_way_it_names_them() {
        let mut g = Graph::default();
        for (id, c) in [
            ("m1aa", "Pip reference"),
            ("m2bb", "the toad"),
            ("m3cc", "the workshop"),
        ] {
            g.media.insert(id.into(), pic(id, c));
        }
        let mut busy = pic("m4dd", "still painting");
        busy.status = MediaStatus::Requested;
        g.media.insert("m4dd".into(), busy);
        let pip = ObjectRev {
            id: "o1".into(),
            rev: "r1".into(),
            name: "Pip".into(),
            description: "a small brass robot".into(),
            refs: vec![RefImage {
                use_for: String::new(),
                media: "m1aa".into(),
                caption: "Pip reference".into(),
            }],
            talks: false,
            voice: None,
            anchor: None,
            removed: false,
            at: 0,
        };
        let mut v = pic("mv", "bloom");
        v.kind = MediaKind::Video;
        // Inline ids (as Gemma writes them) become references, in order.
        v.prompt = "Pip from <m1aa> beside m2bb; starts as <m3cc>, not m4dd".into();
        let (refs, p) = resolve_refs(
            &g,
            &v,
            std::slice::from_ref(&pip),
            9,
            Some("<Picture {n}>"),
            &[],
        );
        assert_eq!(refs, ["m1aa", "m2bb", "m3cc"]);
        // Pip's setup picture, with no "use for": what Pip looks like, nothing more.
        assert_eq!(
            p,
            "<Picture 1>: what Pip looks like, not where it is; the pose, place, light and framing as described here.

Pip from <Picture 1> beside <Picture 2>; starts as <Picture 3>, not still painting"
        );
        // <Picture N> by the LLM's refs order; a picture model names them its own way.
        let mut i = pic("mi", "x");
        i.refs = vec!["m3cc".into(), "m1aa".into()];
        i.prompt = "<Picture 2> in <Picture 1>".into();
        let (refs, p) = resolve_refs(
            &g,
            &i,
            std::slice::from_ref(&pip),
            10,
            Some("<image{n}>"),
            &[],
        );
        assert_eq!(refs, ["m1aa", "m3cc"]);
        assert_eq!(
            p,
            "<image1>: what Pip looks like, not where it is; the pose, place, light and framing as described here.

<image1> in <image2>"
        );
        // A model without references gets names in words.
        let (refs, p) = resolve_refs(&g, &i, std::slice::from_ref(&pip), 0, None, &[]);
        assert!(refs.is_empty());
        assert_eq!(p, "Pip in the workshop");
        // Nothing named: each object's reference picture.
        let mut o = pic("mo", "x");
        o.objects = vec!["pip".into()];
        o.prompt = "a robot at dusk".into();
        assert_eq!(
            resolve_refs(
                &g,
                &o,
                std::slice::from_ref(&pip),
                10,
                Some("<image{n}>"),
                &[]
            )
            .0,
            ["m1aa"]
        );
        // Pictured since on this path: still the author's own picture, with what they
        // wrote it's for (a picture made since drifted, a copy of a copy).
        let mut scene = pic("mscene", "Pip at the bench");
        scene.objects = vec!["Pip".into()];
        g.media.insert("mscene".into(), scene);
        let mut faced = pip.clone();
        faced.refs[0].use_for = "the face".into();
        let (refs, p) = resolve_refs(
            &g,
            &o,
            std::slice::from_ref(&faced),
            10,
            Some("<image{n}>"),
            &["mscene".into()],
        );
        assert_eq!(refs, ["m1aa"]);
        assert!(p.starts_with("<image1>: the face.\n\n"), "{p}");
        // What the author said a reference is for goes with it, by the model's own name.
        let mut face = pip.clone();
        face.refs[0].use_for = "the face".into();
        let (_, p) = resolve_refs(&g, &i, &[face], 10, Some("<image{n}>"), &[]);
        assert!(p.starts_with("<image1>: the face.\n\n"), "{p}");
    }
}
