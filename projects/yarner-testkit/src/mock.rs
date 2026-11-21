//! A mock thinfer: one simulated GPU running jobs in priority order, a scripted
//! storyteller that follows the contract, and placeholder pictures.
//!
//! The storyteller reads the same messages a real model gets, so it exercises the context
//! (rejoin targets, pictures, choices, tools) and the engine's handling of its replies.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use yarner_engine::backend::*;

use crate::png;

/// Simulated job durations.
#[derive(Clone, Debug)]
pub struct Timing {
    pub text: Duration,
    pub image: Duration,
    pub video: Duration,
    pub audio: Duration,
}

impl Timing {
    /// No delay at all (unit tests).
    pub fn instant() -> Self {
        Self {
            text: Duration::ZERO,
            image: Duration::ZERO,
            video: Duration::ZERO,
            audio: Duration::ZERO,
        }
    }

    /// Measured on an RTX 5070 Laptop (2026-11-20): a Gemma beat ~23 s, a z-image picture
    /// ~22 s, a FastH3 clip ~200 s; scaled by `scale`.
    pub fn measured(scale: f64) -> Self {
        let s = |secs: f64| Duration::from_secs_f64(secs * scale);
        Self {
            text: s(23.0),
            image: s(22.0),
            video: s(200.0),
            // A line of speech, model warm (thinfer 7857f42: ~0.65 s a second of speech).
            audio: s(4.0),
        }
    }

    /// Real-world ratios (gemma ~4 s a beat, image ~30 s, video ~200 s) scaled by `scale`.
    pub fn realistic(scale: f64) -> Self {
        let s = |secs: f64| Duration::from_secs_f64(secs * scale);
        Self {
            text: s(4.0),
            image: s(30.0),
            video: s(200.0),
            audio: s(4.0),
        }
    }
}

/// Per-kind counters, for playtest metrics.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub jobs: HashMap<JobKind, u32>,
    pub busy: Duration,
    pub started: Option<Instant>,
    pub cancelled: u32,
    /// Of those, picture and video jobs.
    pub cancelled_media: u32,
    pub tool_calls: u32,
    /// Shot-step turns answered (`yarner_engine::shot`).
    pub shots: u32,
    /// Session text jobs that continued a node, started a new root, or found it gone.
    pub resumed: u32,
    pub roots: u32,
    pub gone: u32,
    /// Text sessions opened.
    pub sessions_opened: u32,
    /// Messages sent per text job, summed (how much each request re-sends).
    pub messages_sent: u64,
    pub text_jobs: u32,
    /// Pictures/videos paused for a preempting job (and resumed later).
    pub paused: u32,
    /// Jobs submitted to cut in (`preempt`).
    pub preempts: u32,
    /// The least urgent job that was sent to cut in.
    pub preempt_min_priority: Option<Priority>,
    /// Each cut-in while something ran: (its priority, the running job's), as they stood.
    pub cut_ins: Vec<(Priority, Priority)>,
    /// Video jobs, and those given at least one reference picture.
    pub videos: u32,
    pub videos_with_refs: u32,
    /// Each video job's length (seconds), as asked.
    pub video_seconds: Vec<f32>,
    /// Prompts of picture and video jobs, as sent.
    pub media_prompts: Vec<String>,
    /// The adapters each picture and video job asked for (ids), as sent.
    pub media_loras: Vec<Vec<String>>,
    /// Per text job, in order: the characters of the whole conversation the model sees
    /// (a session's history plus what was sent).
    pub context_chars: Vec<usize>,
    /// Audio jobs, as sent.
    pub audio: Vec<AudioJob>,
    /// Reference audio clips each video job was given.
    pub video_ref_audios: Vec<usize>,
}

/// An audio job as the mock saw it.
#[derive(Clone, Debug)]
pub struct AudioJob {
    pub model: String,
    pub text: String,
    pub instruct: Option<String>,
    /// Given a voice clip (and what it says).
    pub voice: bool,
    pub voice_words: Option<String>,
    pub seconds: Option<f32>,
    pub looped: bool,
    pub seed: Option<u64>,
}

impl Stats {
    pub fn utilization(&self) -> f64 {
        match self.started {
            Some(s) if s.elapsed() > Duration::ZERO => {
                self.busy.as_secs_f64() / s.elapsed().as_secs_f64()
            }
            _ => 0.0,
        }
    }
}

struct Queued {
    id: JobId,
    job: Job,
    priority: Priority,
    seq: u64,
    /// Run next, pausing a running picture/video (it resumes after this one).
    preempt: bool,
    /// Work left, for a job that was paused partway.
    remaining: Option<Duration>,
    tx: oneshot::Sender<Result<JobOutput, BackendError>>,
    /// A text job's reply as it's "generated".
    stream: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}

#[derive(Default)]
struct Q {
    queued: Vec<Queued>,
    running: Option<(JobId, JobKind, Priority)>,
    /// Cancel asked for the running job (checked between slices of its work).
    cancel_running: bool,
    /// A preempting job arrived: pause the running picture/video.
    pause_running: bool,
    /// Pictures and video can't pause (like a backend without that).
    no_pause: bool,
    seq: u64,
    stats: Stats,
    /// Session nodes: the whole conversation up to and including that reply.
    nodes: HashMap<String, Vec<Message>>,
    sessions: u64,
    /// Drop every session node after this many session jobs (0 = never).
    evict_every: u64,
    /// Keep at most this many session nodes, letting the least recently used go (like
    /// thinfer's session RAM budget; 0 = no limit).
    node_budget: usize,
    /// Session nodes, least recently used first.
    lru: std::collections::VecDeque<String>,
    session_jobs: u64,
    /// Fail every Nth text job after streaming its reply (0 = never).
    fail_every: u64,
    /// One new picture in this many is a video (0 = the default, 15).
    video_every: u64,
    /// The running job's start and length (for progress).
    clock: Option<(JobId, Instant, Duration)>,
    /// The kind of the last job run, and until when the running one is "loading its
    /// model" (the first fifth of a job after a switch of kind, like a model swap).
    last_kind: Option<JobKind>,
    loading_until: Option<(JobId, Instant)>,
    /// The adapter vault's password (memory only, like thinfer's client side).
    vault: Option<String>,
    /// End the story once this many beats have been written on a path (0 = never).
    end_at: u64,
    /// The next shot replies come back thin (to be asked again).
    thin_shots: u32,
    /// Names the shot writer leaves out of view (the player behind the camera).
    hidden: Vec<String>,
    /// Names the shot writer shows only in part (a hand at the edge of the frame).
    partly: Vec<String>,
    /// Fail this many of the next picture/video jobs.
    fail_media: u32,
    /// Refuse every job, as if the server were unreachable.
    down: bool,
}

pub struct MockThinfer {
    q: Arc<Mutex<Q>>,
    wake: Arc<Notify>,
    /// Fail every Nth text job with garbage (exercises retries). 0 = never.
    pub garbage_every: u64,
}

impl MockThinfer {
    pub fn new(timing: Timing) -> Arc<Self> {
        let q = Arc::new(Mutex::new(Q::default()));
        let wake = Arc::new(Notify::new());
        q.lock().unwrap().stats.started = Some(Instant::now());
        let me = Arc::new(Self {
            q: q.clone(),
            wake: wake.clone(),
            garbage_every: 0,
        });
        tokio::spawn(gpu(q, wake, timing, 0));
        me
    }

    /// As [`MockThinfer::new`], with every `n`th text reply invalid.
    pub fn with_garbage(timing: Timing, n: u64) -> Arc<Self> {
        let q = Arc::new(Mutex::new(Q::default()));
        let wake = Arc::new(Notify::new());
        q.lock().unwrap().stats.started = Some(Instant::now());
        tokio::spawn(gpu(q.clone(), wake.clone(), timing, n));
        Arc::new(Self {
            q,
            wake,
            garbage_every: n,
        })
    }

    /// Keep only the `n` most recently used session nodes (thinfer's session budget).
    pub fn session_node_budget(&self, n: usize) {
        self.q.lock().unwrap().node_budget = n;
    }

    /// Forget every session node every `n` session jobs (exercises the resend fallback).
    pub fn evict_sessions_every(&self, n: u64) {
        self.q.lock().unwrap().evict_every = n;
    }

    /// Fail every `n`th text job after its reply has streamed (a crash mid-generation).
    pub fn fail_text_every(&self, n: u64) {
        self.q.lock().unwrap().fail_every = n;
    }

    /// Pictures and video can't pause (a waiting reader's text queues behind them).
    pub fn no_pause(&self) {
        self.q.lock().unwrap().no_pause = true;
    }

    /// End the story once a path has `n` beats (the storyteller says "end").
    pub fn end_at(&self, n: u64) {
        self.q.lock().unwrap().end_at = n;
    }

    /// The shot writer leaves `name` out of view (as a first-person shot leaves the
    /// player behind the camera).
    pub fn shot_hides(&self, name: &str) {
        self.q.lock().unwrap().hidden.push(name.to_string());
    }

    /// The shot writer shows `name` only in part (their hand at the edge of the frame).
    pub fn shot_partly(&self, name: &str) {
        self.q.lock().unwrap().partly.push(name.to_string());
    }

    /// The next `n` shot-step replies come back thin (one-word fields).
    pub fn thin_shots(&self, n: u32) {
        self.q.lock().unwrap().thin_shots = n;
    }

    /// Fail the next `n` picture/video jobs.
    pub fn fail_media(&self, n: u32) {
        self.q.lock().unwrap().fail_media = n;
    }

    /// Refuse every job (unreachable server) until set back.
    pub fn set_down(&self, down: bool) {
        self.q.lock().unwrap().down = down;
        self.wake.notify_one();
    }

    /// Make one new picture in `n` a video.
    pub fn video_every(&self, n: u64) {
        self.q.lock().unwrap().video_every = n;
    }

    pub fn stats(&self) -> Stats {
        self.q.lock().unwrap().stats.clone()
    }
}

async fn gpu(q: Arc<Mutex<Q>>, wake: Arc<Notify>, timing: Timing, garbage_every: u64) {
    let mut text_n = 0u64;
    loop {
        let next = {
            let mut q = q.lock().unwrap();
            // A preempting job first; then highest priority, then oldest.
            let best = q
                .queued
                .iter()
                .enumerate()
                .max_by_key(|(_, j)| (j.preempt, j.priority, std::cmp::Reverse(j.seq)))
                .map(|(i, _)| i);
            best.map(|i| {
                let j = q.queued.remove(i);
                q.running = Some((j.id.clone(), j.job.kind(), j.priority));
                j
            })
        };
        let Some(j) = next else {
            if Arc::strong_count(&q) == 1 {
                return;
            }
            let _ = tokio::time::timeout(Duration::from_millis(200), wake.notified()).await;
            continue;
        };
        let kind = j.job.kind();
        let d = j.remaining.unwrap_or(match kind {
            JobKind::Text => timing.text,
            JobKind::Image => timing.image,
            JobKind::Video => timing.video,
            JobKind::Audio => timing.audio,
        });
        let t0 = Instant::now();
        {
            let mut q = q.lock().unwrap();
            q.clock = Some((j.id.clone(), t0, d));
            // A fresh start after a switch loads its model first (a resumed job doesn't).
            q.loading_until = (q.last_kind != Some(kind) && j.remaining.is_none())
                .then(|| (j.id.clone(), t0 + d.mul_f64(0.2)));
            q.last_kind = Some(kind);
        }
        // Text streams its reply over the last 40% of the job (decoding after prefill).
        let stream_part = if kind == JobKind::Text {
            d.mul_f64(0.4)
        } else {
            Duration::ZERO
        };
        let mut cancelled = false;
        if !d.is_zero() {
            match nap(&q, d - stream_part, kind != JobKind::Text).await {
                Nap::Done => {}
                Nap::Cancelled => cancelled = true,
                Nap::Paused(left) => {
                    // Back in the queue with the work it has left; nothing is lost. At
                    // the priority it has now (it may have changed while it ran; the
                    // one it started with made it look like a 964 picture was paused by
                    // a 950 when the engine had long made it -100).
                    let mut q = q.lock().unwrap();
                    let priority = q.running.take().map_or(j.priority, |r| r.2);
                    q.pause_running = false;
                    q.stats.paused += 1;
                    q.stats.busy += t0.elapsed();
                    q.queued.push(Queued {
                        remaining: Some(left),
                        preempt: false,
                        priority,
                        ..j
                    });
                    continue;
                }
            }
        } else {
            tokio::task::yield_now().await;
        }
        let out = match j.job {
            Job::Text(mut req) => {
                text_n += 1;
                // Sessions: rebuild the whole conversation from the node it continues.
                let mut node_base = None;
                {
                    let mut q = q.lock().unwrap();
                    q.stats.messages_sent += req.messages.len() as u64;
                    q.stats.text_jobs += 1;
                }
                if let Some(r) = req.session.clone() {
                    let mut q = q.lock().unwrap();
                    q.session_jobs += 1;
                    if q.evict_every > 0 && q.session_jobs.is_multiple_of(q.evict_every) {
                        q.nodes.clear();
                        q.lru.clear();
                    }
                    let base = match &r.from {
                        None => Some(vec![]),
                        Some(n) => q.nodes.get(n).cloned(),
                    };
                    if let (Some(n), Some(_)) = (&r.from, &base) {
                        q.lru.retain(|x| x != n);
                        q.lru.push_back(n.clone());
                    }
                    match (&r.from, &base) {
                        (None, _) => q.stats.roots += 1,
                        (Some(_), Some(_)) => q.stats.resumed += 1,
                        (Some(_), None) => q.stats.gone += 1,
                    }
                    match base {
                        None => {
                            drop(q);
                            let _ = j.tx.send(Err(BackendError::SessionGone));
                            continue;
                        }
                        Some(mut b) => {
                            b.append(&mut req.messages);
                            req.messages = b;
                            node_base = Some(req.messages.clone());
                        }
                    }
                }
                {
                    let seen = req.messages.iter().map(|m| m.content.len()).sum();
                    q.lock().unwrap().stats.context_chars.push(seen);
                }
                if garbage_every > 0 && text_n.is_multiple_of(garbage_every) {
                    Ok(JobOutput::Text(TextResponse {
                        content: "I think the story should continue with".into(),
                        ..Default::default()
                    }))
                } else {
                    let (video_every, end_at) = {
                        let q = q.lock().unwrap();
                        (
                            match q.video_every {
                                0 => 15,
                                n => n,
                            },
                            q.end_at,
                        )
                    };
                    // The shot turn, or the ask-again right after it.
                    let shot_turn = req.messages.iter().rev().take(3).any(|m| {
                        m.role == "user" && m.content.contains(yarner_engine::shot::HEADING)
                    });
                    let mut r = if shot_turn {
                        let mut q = q.lock().unwrap();
                        q.stats.shots += 1;
                        let thin = q.thin_shots > 0;
                        if thin {
                            q.thin_shots -= 1;
                        }
                        let (hidden, partly) = (q.hidden.clone(), q.partly.clone());
                        shot_writer(&req, thin, &hidden, &partly)
                    } else {
                        storyteller_with(&req, video_every)
                    };
                    if end_at > 0 {
                        // Beats so far on this path: the replies in this conversation.
                        let n = req
                            .messages
                            .iter()
                            .filter(|m| m.role == "assistant")
                            .count() as u64;
                        // Only where the schema lets the story end.
                        let may_end = req
                            .schema
                            .as_ref()
                            .is_none_or(|s| s.to_string().contains("\"const\":\"end\""));
                        if n + 1 >= end_at
                            && !shot_turn
                            && may_end
                            && let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&r.content)
                        {
                            v["next"] = serde_json::json!("end");
                            r.content = v.to_string();
                        }
                    }
                    let mut q = q.lock().unwrap();
                    if !r.tool_calls.is_empty() {
                        q.stats.tool_calls += 1;
                    }
                    if let Some(mut conv) = node_base {
                        let mut reply = Message::new("assistant", r.content.clone());
                        reply.tool_calls = r.tool_calls.clone();
                        conv.push(reply);
                        q.seq += 1;
                        let id = format!("node{}", q.seq);
                        q.nodes.insert(id.clone(), conv);
                        q.lru.push_back(id.clone());
                        while q.node_budget > 0 && q.lru.len() > q.node_budget {
                            let old = q.lru.pop_front().expect("non-empty");
                            q.nodes.remove(&old);
                        }
                        r.session_node = Some(id);
                    }
                    Ok(JobOutput::Text(r))
                }
            }
            Job::Image(req) => {
                q.lock()
                    .unwrap()
                    .stats
                    .media_prompts
                    .push(req.prompt.clone());
                q.lock()
                    .unwrap()
                    .stats
                    .media_loras
                    .push(req.loras.iter().map(|l| l.id.clone()).collect());
                Ok(picture(&req.prompt, req.width, req.height, req.refs.len()))
            }
            Job::Video(req) => {
                {
                    let mut q = q.lock().unwrap();
                    q.stats.media_prompts.push(req.prompt.clone());
                    q.stats
                        .media_loras
                        .push(req.loras.iter().map(|l| l.id.clone()).collect());
                    q.stats.videos += 1;
                    q.stats.video_seconds.push(req.seconds);
                    q.stats.video_ref_audios.push(req.ref_audios.len());
                    if !req.refs.is_empty() {
                        q.stats.videos_with_refs += 1;
                    }
                }
                Ok(picture(&req.prompt, req.width, req.height, req.refs.len()))
            }
            Job::Audio(req) => {
                let secs = req.seconds.unwrap_or(1.0);
                q.lock().unwrap().stats.audio.push(AudioJob {
                    model: req.model.clone(),
                    text: req.text.clone(),
                    instruct: req.instruct.clone(),
                    voice: req.voice.is_some(),
                    voice_words: req.voice_words.clone(),
                    seconds: req.seconds,
                    looped: req.looped,
                    seed: req.seed,
                });
                Ok(JobOutput::Media {
                    bytes: wav(secs.min(2.0)),
                    mime: "audio/wav".into(),
                })
            }
        };
        if let (Some(tx), Ok(JobOutput::Text(t))) = (&j.stream, &out) {
            let chars: Vec<char> = t.content.chars().collect();
            let pieces = 6;
            let size = chars.len().div_ceil(pieces).max(1);
            for c in chars.chunks(size) {
                if cancelled {
                    break;
                }
                let _ = tx.send(c.iter().collect());
                if !stream_part.is_zero() {
                    cancelled = nap(&q, stream_part / pieces as u32, false).await == Nap::Cancelled;
                }
            }
        }
        let out = {
            let mut q = q.lock().unwrap();
            if cancelled || q.cancel_running {
                q.stats.cancelled += 1;
                // Not audio: a sound is dropped for a waiting reader by design (made
                // again later), a picture never is.
                if matches!(kind, JobKind::Image | JobKind::Video) {
                    q.stats.cancelled_media += 1;
                }
                Err(BackendError::Cancelled)
            } else if kind == JobKind::Text
                && q.fail_every > 0
                && text_n.is_multiple_of(q.fail_every)
            {
                Err(BackendError::Failed("mock: crashed mid-generation".into()))
            } else if kind != JobKind::Text && q.fail_media > 0 {
                q.fail_media -= 1;
                Err(BackendError::Failed("mock: out of memory".into()))
            } else {
                out
            }
        };
        {
            let mut q = q.lock().unwrap();
            q.running = None;
            q.cancel_running = false;
            *q.stats.jobs.entry(kind).or_default() += 1;
            q.stats.busy += t0.elapsed();
        }
        let _ = j.tx.send(out);
    }
}

#[derive(Debug, PartialEq)]
enum Nap {
    Done,
    Cancelled,
    /// Paused (`pausable` jobs only), with this much work left.
    Paused(Duration),
}

/// Work for `d`, in slices, unless the job is cancelled or (if `pausable`) paused.
async fn nap(q: &Arc<Mutex<Q>>, d: Duration, pausable: bool) -> Nap {
    let end = Instant::now() + d;
    loop {
        {
            let q = q.lock().unwrap();
            if q.cancel_running {
                return Nap::Cancelled;
            }
            if pausable && q.pause_running {
                return Nap::Paused(end.saturating_duration_since(Instant::now()));
            }
        }
        let now = Instant::now();
        if now >= end {
            return Nap::Done;
        }
        tokio::time::sleep((end - now).min(Duration::from_millis(2))).await;
    }
}

fn picture(prompt: &str, w: u32, h: u32, refs: usize) -> JobOutput {
    let seed = hash(&(prompt, refs));
    JobOutput::Media {
        bytes: png::placeholder((w / 8).max(8), (h / 8).max(8), seed),
        mime: "image/png".into(),
    }
}

fn hash<T: Hash>(t: &T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    t.hash(&mut h);
    h.finish()
}

impl Backend for MockThinfer {
    fn submit(
        &self,
        job: Job,
        priority: Priority,
        preempt: bool,
    ) -> BoxFuture<'_, Result<Submitted, BackendError>> {
        Box::pin(async move {
            if self.q.lock().unwrap().down {
                return Err(BackendError::Failed("mock: connection refused".into()));
            }
            let (tx, rx) = oneshot::channel();
            let (stream, text) = if matches!(job, Job::Text(_)) {
                let (s, r) = tokio::sync::mpsc::unbounded_channel();
                (Some(s), Some(r))
            } else {
                (None, None)
            };
            let id = {
                let mut q = self.q.lock().unwrap();
                q.seq += 1;
                let id = format!("job{}", q.seq);
                let seq = q.seq;
                q.queued.push(Queued {
                    id: id.clone(),
                    job,
                    priority,
                    seq,
                    preempt,
                    remaining: None,
                    tx,
                    stream,
                });
                if preempt {
                    q.stats.preempts += 1;
                    let m = q.stats.preempt_min_priority.get_or_insert(priority);
                    *m = (*m).min(priority);
                    if let Some((_, _, p)) = q.running {
                        q.stats.cut_ins.push((priority, p));
                    }
                }
                // Like thinfer: a running picture/video pauses for it.
                if preempt
                    && !q.no_pause
                    && matches!(q.running, Some((_, k, _)) if k != JobKind::Text)
                {
                    q.pause_running = true;
                }
                id
            };
            self.wake.notify_one();
            Ok(Submitted {
                id,
                done: Box::pin(async move { rx.await.unwrap_or(Err(BackendError::Cancelled)) }),
                text,
            })
        })
    }

    fn reprioritize(&self, id: &JobId, priority: Priority) -> BoxFuture<'_, ()> {
        let id = id.clone();
        Box::pin(async move {
            if let Some(j) = self
                .q
                .lock()
                .unwrap()
                .queued
                .iter_mut()
                .find(|j| j.id == id)
            {
                j.priority = priority;
            }
            let mut q = self.q.lock().unwrap();
            if let Some(r) = q.running.as_mut().filter(|r| r.0 == id) {
                r.2 = priority;
            }
        })
    }

    fn cancel(&self, id: &JobId) -> BoxFuture<'_, ()> {
        let id = id.clone();
        Box::pin(async move {
            let mut q = self.q.lock().unwrap();
            if let Some(i) = q.queued.iter().position(|j| j.id == id) {
                let j = q.queued.remove(i);
                q.stats.cancelled += 1;
                let _ = j.tx.send(Err(BackendError::Cancelled));
            } else if q.running.as_ref().is_some_and(|r| r.0 == id) {
                // Stops at its next step, like thinfer.
                q.cancel_running = true;
            }
        })
    }

    fn queue(&self) -> BoxFuture<'_, Vec<QueueEntry>> {
        Box::pin(async move {
            let q = self.q.lock().unwrap();
            let mut v: Vec<QueueEntry> = q
                .queued
                .iter()
                .map(|j| QueueEntry {
                    id: j.id.clone(),
                    kind: Some(j.job.kind()),
                    priority: j.priority,
                    running: false,
                    progress: None,
                    what: None,
                    ..Default::default()
                })
                .collect();
            if let Some((id, kind, priority)) = &q.running {
                v.push(QueueEntry {
                    id: id.clone(),
                    kind: Some(*kind),
                    priority: *priority,
                    running: true,
                    progress: None,
                    what: None,
                    ..Default::default()
                });
            }
            v
        })
    }

    fn create_session(&self, _model: &str) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let mut q = self.q.lock().unwrap();
            q.sessions += 1;
            q.stats.sessions_opened += 1;
            Some(format!("session{}", q.sessions))
        })
    }

    fn drop_session(&self, _session: &str) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn started(&self, id: &JobId) -> bool {
        // Like thinfer: taken by the (one) worker; a job not in the line any more has.
        let q = self.q.lock().unwrap();
        q.running.as_ref().is_some_and(|r| &r.0 == id) || !q.queued.iter().any(|j| &j.id == id)
    }

    fn queued(&self, id: &JobId) -> Option<u32> {
        let q = self.q.lock().unwrap();
        let mut line: Vec<&Queued> = q.queued.iter().collect();
        line.sort_by_key(|j| (std::cmp::Reverse(j.priority), j.seq));
        line.iter().position(|j| &j.id == id).map(|i| i as u32 + 1)
    }

    fn set_vault(&self, password: Option<String>) {
        self.q.lock().unwrap().vault = password.filter(|p| !p.is_empty());
    }

    fn vault_set(&self) -> bool {
        self.q.lock().unwrap().vault.is_some()
    }

    /// The mock's vault holds two adapters for every picture and video model.
    fn adapters(&self, model: String) -> BoxFuture<'_, Option<Vec<Adapter>>> {
        let open = self.q.lock().unwrap().vault.is_some();
        Box::pin(async move {
            open.then(|| {
                vec![
                    Adapter {
                        id: format!("{model}-ink"),
                        name: "Ink wash".into(),
                        weight: Some(0.8),
                    },
                    Adapter {
                        id: format!("{model}-grain"),
                        name: "Film grain".into(),
                        weight: None,
                    },
                ]
            })
        })
    }

    /// The mock's vault opens with any password but one starting "wrong".
    fn vault_check(&self) -> BoxFuture<'_, Option<bool>> {
        let pw = self.q.lock().unwrap().vault.clone();
        Box::pin(async move { pw.map(|p| !p.starts_with("wrong")) })
    }

    fn loading(&self, id: &JobId) -> bool {
        let q = self.q.lock().unwrap();
        q.loading_until
            .as_ref()
            .is_some_and(|(j, until)| j == id && Instant::now() < *until)
    }

    fn progress(&self, id: &JobId) -> Option<f32> {
        let q = self.q.lock().unwrap();
        let (run, t0, d) = q.clock.as_ref()?;
        (run == id && !d.is_zero())
            .then(|| (t0.elapsed().as_secs_f32() / d.as_secs_f32()).min(0.99))
    }

    fn models(&self) -> BoxFuture<'_, Vec<yarner_engine::backend::ModelCaps>> {
        let pausable = !self.q.lock().unwrap().no_pause;
        Box::pin(async move {
            use yarner_engine::backend::ModelCaps;
            let m = |id: &str, kind, refs: u32, tag: Option<&str>, pause: bool| ModelCaps {
                id: id.into(),
                kind: Some(kind),
                title: format!("mock {id}"),
                default: false,
                downloaded: true,
                pausable: pause,
                max_refs: refs,
                ref_tag: tag.map(Into::into),
                size_multiple: 16,
                default_size: Some((640, 352)),
                max_ref_audios: 0,
                audio_tag: None,
                makes_audio: false,
                audio: None,
            };
            vec![
                m("gemma-4-26b-a4b", JobKind::Text, 0, None, false),
                m(
                    "qwen-image-2.1-turbo",
                    JobKind::Image,
                    10,
                    Some("<image{n}>"),
                    pausable,
                ),
                m("zimage-turbo-q4", JobKind::Image, 0, None, pausable),
                ModelCaps {
                    makes_audio: true,
                    ..m("fasth3-trim", JobKind::Video, 0, None, pausable)
                },
                ModelCaps {
                    makes_audio: true,
                    max_ref_audios: 3,
                    audio_tag: Some("<Audio {n}>".into()),
                    ..m(
                        "fasth3-omni",
                        JobKind::Video,
                        9,
                        Some("<Picture {n}>"),
                        pausable,
                    )
                },
                // A video model with no soundtrack.
                m("fastwan-ti2v-5b", JobKind::Video, 0, None, pausable),
                ModelCaps {
                    makes_audio: true,
                    audio: Some(yarner_engine::backend::AudioCaps {
                        speech: true,
                        instruct: false,
                        clone: true,
                        max_seconds: 120.0,
                    }),
                    ..m("qwen3-tts", JobKind::Audio, 0, None, false)
                },
                ModelCaps {
                    makes_audio: true,
                    audio: Some(yarner_engine::backend::AudioCaps {
                        speech: true,
                        instruct: true,
                        clone: false,
                        max_seconds: 120.0,
                    }),
                    ..m("qwen3-tts-design", JobKind::Audio, 0, None, false)
                },
                ModelCaps {
                    makes_audio: true,
                    audio: Some(yarner_engine::backend::AudioCaps {
                        speech: false,
                        instruct: false,
                        clone: false,
                        max_seconds: 30.0,
                    }),
                    ..m("moss-sfx", JobKind::Audio, 0, None, false)
                },
            ]
        })
    }
}

/// Lines under a `# HEADING` in the user message.
fn section<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut on = false;
    for l in text.lines() {
        if l.starts_with("# ") {
            on = l.starts_with(heading);
            continue;
        }
        if on && !l.trim().is_empty() {
            out.push(l);
        }
    }
    out
}

fn first_word_id(line: &str, prefix: char) -> Option<String> {
    let t = line.trim_start_matches("- ");
    let id: String = t
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ':')
        .collect();
    id.starts_with(prefix).then_some(id)
}

const PLACES: &[&str] = &[
    "the old pier",
    "a lantern-lit alley",
    "the clifftop",
    "the market square",
    "a dusty archive",
];
const SAYINGS: &[&str] = &[
    "I have kept this lamp burning for eleven winters and never once seen it flicker.",
    "Whatever you heard down there, it was not the tide. The tide does not knock.",
    "The ledger stops on the third of March. After that, only drawings of circles.",
    "If we wait until dawn the boat will be gone, and so will our only way back.",
    "She folded the map twice, then a third time, as if hiding it from herself.",
    "Somewhere below the floorboards a bell rang once, very softly, and stopped.",
    "You can trust me or you can trust the sea, but not both on the same night.",
    "The wind changed. Every gull on the rocks turned to face the water at once.",
];
const ACTS: &[&str] = &[
    "listens to the wind",
    "finds a torn map",
    "hears footsteps",
    "notices a light flicker",
    "remembers a promise",
];

/// The scripted storyteller: a valid beat for the request, varied by its content.
pub fn storyteller(req: &TextRequest) -> TextResponse {
    storyteller_with(req, 15)
}

/// As [`storyteller`], with one new picture in `video_every` a video.
pub fn storyteller_with(req: &TextRequest, video_every: u64) -> TextResponse {
    let sys = req
        .messages
        .first()
        .map(|m| m.content.as_str())
        .unwrap_or("");
    // The latest user turn (in a chat, earlier turns are history).
    let user = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("");
    let replies = req
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .count() as u64;
    let used_tool = req.messages.iter().any(|m| m.role == "tool");
    let seed = hash(&(user, req.messages.len()));
    let roll =
        |salt: u64, n: u64| (seed.rotate_left(salt as u32 * 7) ^ salt.wrapping_mul(0x9e37)) % n;

    let names: Vec<String> = section(sys, "## Objects")
        .iter()
        .filter_map(|l| l.strip_prefix("- "))
        .filter_map(|l| l.split(':').next())
        .map(str::to_string)
        .collect();
    // The objects heading is a `##`, which `section` treats as a non-heading line; parse
    // it directly instead.
    let names: Vec<String> = if names.is_empty() {
        let mut on = false;
        let mut v = Vec::new();
        for l in sys.lines() {
            if l.starts_with("## Objects") {
                on = true;
                continue;
            }
            if l.starts_with('#') {
                on = false;
            }
            if on && let Some(n) = l.strip_prefix("- ").and_then(|l| l.split(':').next()) {
                v.push(n.to_string());
            }
        }
        v
    } else {
        names
    };
    let mut pic_lines = section(user, "# PICTURES");
    pic_lines.extend(section(user, "# NEW PICTURES"));
    let mut pics: Vec<String> = pic_lines
        .iter()
        .filter(|l| !l.contains("still being made"))
        .filter_map(|l| first_word_id(l, 'm'))
        .collect();
    // Like a constrained decoder: only pictures the schema lets it show again.
    if let Some(allowed) = req.schema.as_ref().and_then(|s| {
        s["properties"]["media"]["anyOf"]
            .as_array()?
            .iter()
            .find_map(|o| o["properties"]["reuse"]["enum"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
    }) {
        pics.retain(|p| allowed.contains(p));
        if pics.is_empty() {
            pics = allowed.into_iter().take(4).collect();
        }
    }
    // References are images only (videos can be shown again, not drawn from), from
    // anywhere in the conversation (a model sees all of it), newest first.
    let mut images: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .flat_map(|m| m.content.lines())
        .filter(|l| l.contains("[image]"))
        .filter_map(|l| first_word_id(l, 'm'))
        .collect();
    images.reverse();
    let on_screen = user.lines().any(|l| l.starts_with("On screen now:"));
    let rejoin: Vec<String> = section(user, "# REJOIN TARGETS")
        .iter()
        .filter_map(|l| first_word_id(l, 'b'))
        .collect();
    let recent = section(user, "# RECENT BEATS");
    let n = recent.len() as u64 + replies;
    let chose = user
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("(The player chose: "));
    let opening = user.contains("Begin the story") && replies == 0;
    // Like a constrained decoder: only what the schema allows.
    let allows = |what: &str| {
        req.schema
            .as_ref()
            .is_none_or(|s| s.to_string().contains(what))
    };
    let allow_choices = allows("\"choices\"");
    let allow_new = allows("\"image\"");
    let allow_keep = allows("\"const\":\"keep\"");

    // Sometimes look things up first, like a real model might.
    if !used_tool && !req.tools.is_empty() && !names.is_empty() && roll(1, 6) == 0 {
        let q = &names[roll(2, names.len() as u64) as usize];
        return TextResponse {
            tool_calls: vec![ToolCall {
                id: format!("call{}", seed % 1000),
                name: "search_images".into(),
                arguments: json!({ "query": q }),
            }],
            ..Default::default()
        };
    }

    let speaker = |i: u64| -> Value {
        if names.is_empty() || roll(10 + i, 3) == 0 {
            Value::Null
        } else {
            json!(names[roll(20 + i, names.len() as u64) as usize])
        }
    };
    // How a line is said: one of the deliveries the schema offers, now and then.
    let tones: Vec<String> = req
        .schema
        .as_ref()
        .and_then(|s| {
            s["properties"]["lines"]["items"]["properties"]["tone"]["enum"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
        })
        .unwrap_or_default();
    let allow_sounds = req
        .schema
        .as_ref()
        .is_some_and(|s| s["properties"].get("sounds").is_some());
    let place = PLACES[roll(3, PLACES.len() as u64) as usize];
    let act = ACTS[roll(4, ACTS.len() as u64) as usize];
    let mut lines =
        vec![json!({"speaker": Value::Null, "text": format!("At {place}, someone {act}.")})];
    if let Some(c) = chose {
        lines.insert(0, json!({"speaker": Value::Null, "text": format!("You decide: {}", c.trim_end_matches(')'))}));
    }
    // Line lengths like a real model's (40-90 characters, 2-5 lines a beat).
    for i in 0..(2 + roll(5, 3)) {
        let say = SAYINGS[roll(30 + i, SAYINGS.len() as u64) as usize];
        let mut l = json!({"speaker": speaker(i), "text": format!("{say} ({n})")});
        if !l["speaker"].is_null() && !tones.is_empty() && roll(40 + i, 2) == 0 {
            l["tone"] = json!(tones[roll(50 + i, tones.len() as u64) as usize]);
        }
        lines.push(l);
    }

    let objs: Vec<&String> = names.iter().take(1 + roll(6, 2) as usize).collect();
    let media = if allow_new && (!on_screen || opening || !allow_keep || roll(7, 5) == 0) {
        if !opening && roll(8, video_every) == 0 {
            // Like a model following the contract: name the ref and say how it's used.
            let refs: Vec<String> = images.iter().take(1).cloned().collect();
            let prompt = if refs.is_empty() {
                format!("{place}, {act}, cinematic")
            } else {
                format!("Starts exactly as <Picture 1>, then {act} at {place}")
            };
            // A short clip, as a storyteller asks for a quick moment.
            json!({"video": {"prompt": prompt, "caption": format!("video at {place}"), "objects": objs, "refs": refs, "seconds": 3}})
        } else {
            json!({"image": {"prompt": format!("{place}, {act}"), "caption": place.to_string(), "objects": objs, "refs": images.iter().take(1).collect::<Vec<_>>()}})
        }
    } else if !pics.is_empty() && (!allow_keep || roll(9, 3) == 0) {
        json!({"reuse": pics[roll(11, pics.len() as u64) as usize]})
    } else {
        json!("keep")
    };

    // Like a model: flow back into an offered line now and then (the engine only offers
    // them once a branch has had its own beats).
    let next = if !rejoin.is_empty() && roll(12, 3) == 0 {
        json!({"rejoin": rejoin[0]})
    } else if allow_choices && (opening || roll(13, 3) == 0) {
        json!({"choices": [
            {"label": format!("Go toward {}", PLACES[roll(14, 5) as usize]), "likely": 0.6},
            {"label": "Wait and watch", "likely": 0.3},
            {"label": "Ask about the light", "likely": 0.1},
        ]})
    } else {
        json!("continue")
    };

    let mut beat = json!({
        "lines": lines,
        "media": media,
        "next": next,
        "label": if opening || roll(15, 6) == 0 { json!(place) } else { Value::Null },
        "summary": if roll(16, 6) == 0 { json!(format!("Things happened at {place}.")) } else { Value::Null },
    });
    if roll(17, 4) == 0 {
        beat["state"] = json!({"set": {format!("seen {place}"): "yes"}});
    }
    // A sound now and then (rare, as the guide asks): a background one when a scene
    // starts, else a sound effect.
    // (The opening, even after a lookup: `opening` counts only a first reply.)
    let first = user.contains("Begin the story");
    if allow_sounds && (first || roll(18, 5) == 0) {
        beat["sounds"] = if first || roll(19, 2) == 0 {
            json!([{"sound": format!("steady wind at {place}"), "line": 0, "loop": true}])
        } else {
            json!([{"sound": "a heavy wooden door creaks open", "line": 1}])
        };
    }

    TextResponse {
        content: beat.to_string(),
        prompt_tokens: (sys.len() + user.len()) as u32 / 4,
        generated_tokens: 120,
        ..Default::default()
    }
}

/// The shot step's writer: every field filled from what the turn lists (who it said is
/// in it, their descriptions), or one-word fields when `thin`.
pub fn shot_writer(
    req: &TextRequest,
    thin: bool,
    hidden: &[String],
    partly: &[String],
) -> TextResponse {
    // The shot turn itself (an ask-again follows it).
    let turn = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user" && m.content.contains(yarner_engine::shot::HEADING))
        .map(|m| m.content.as_str())
        .unwrap_or("");
    let video = req
        .schema
        .as_ref()
        .is_some_and(|s| s["properties"].get("motion").is_some());
    // Who it said is in it, then everyone the turn lists as maybe in view (each with its
    // description as it stands here).
    let mut shows: Vec<String> = turn
        .lines()
        .find_map(|l| l.strip_prefix("You said it shows: "))
        .map(|l| l.split(", ").map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    for name in section(turn, "# WHO AND WHAT MAY BE IN VIEW")
        .iter()
        .filter_map(|l| {
            l.strip_prefix("- ")?
                .split_once(": ")
                .map(|(n, _)| n.to_string())
        })
    {
        if !shows.contains(&name) {
            shows.push(name);
        }
    }
    shows.retain(|n| !hidden.contains(n));
    let idea = turn
        .lines()
        .find_map(|l| l.strip_prefix("Your idea for it: "))
        .unwrap_or("a basalt spit")
        .trim()
        .trim_end_matches('.')
        .to_string();
    let desc = |name: &str| -> String {
        turn.lines()
            .find_map(|l| l.strip_prefix(&format!("- {name}: ")))
            .unwrap_or("a figure in a long grey travelling coat with a satchel")
            .to_string()
    };
    let mut v = if thin {
        json!({
            "camera": "close", "subjects": [], "place": "outside", "details": "",
            "mood": "calm",
        })
    } else {
        let subjects: Vec<Value> = shows
            .iter()
            .map(|n| {
                json!({
                    "who": n,
                    "in_view": if partly.contains(n) { "part" } else { "whole" },
                    "look": format!("{}, exactly as described, in muted blues and greys, weathered by salt wind", desc(n)),
                    "doing": "standing at the left of the frame, looking out to sea with a calm, wary expression",
                })
            })
            .collect();
        json!({
            "camera": "Medium wide shot at eye level, the subject on the left third, the sea on the right.",
            "subjects": subjects,
            // Built on the beat's own idea, as a real writer's shot is: each one differs
            // (a fixed text made every shot look the same, so each was shown again
            // instead of painted).
            "place": format!("{idea}, at dusk under a heavy sky, cold blue light from the west"),
            "details": format!("In the moment: {idea}."),
            "mood": "Lonely and tense; slate blues and bone white.",
        })
    };
    if video {
        v["motion"] = json!(if thin {
            "moves"
        } else {
            "The surf rolls in and breaks against the rocks, a coat flaps in the wind, the camera pushes in slowly."
        });
        if !thin {
            v["timeline"] = json!([
                {"at": "0-2 s", "what": "The surf rolls in and bursts white over the black rocks"},
                {"at": "2-4 s", "what": "A coat flaps hard in a gust as gulls wheel overhead"},
                {"at": "4-5 s", "what": "The camera settles on the tower as the light fades"},
            ]);
        }
        v["sound"] = json!(if thin {
            ""
        } else {
            "Waves crashing, wind gusting, gulls crying, no music."
        });
    }
    TextResponse {
        content: v.to_string(),
        ..Default::default()
    }
}

/// A short mono 16-bit WAV (8 kHz): a quiet tone, like a clip of speech or a sound.
pub fn wav(seconds: f32) -> Vec<u8> {
    let rate = 8000u32;
    let n = (seconds.max(0.1) * rate as f32) as u32;
    let data = n * 2;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 2000.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}
