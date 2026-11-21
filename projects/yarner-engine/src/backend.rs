//! What the engine needs from a generator (thinfer-serve, or a mock).
//!
//! Jobs are submitted with a priority and can be re-prioritized or cancelled while queued.
//! The shapes mirror thinfer's wire types closely so the real client is a thin mapping.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Higher runs first.
pub type Priority = i32;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// A tool result: the name of the tool that was called (the call itself may not be
    /// in what's sent, when a session continues from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            name: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TextRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    /// JSON Schema the final reply must follow.
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// Reason before answering (thinking mode).
    #[serde(default)]
    pub think: bool,
    /// Continue a session (KV cache kept by the backend): `messages` then holds only the
    /// turns after `from` (all of them when `from` is `None`, a new root).
    #[serde(default)]
    pub session: Option<SessionRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    /// From [`Backend::create_session`].
    pub session: String,
    pub from: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TextResponse {
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub generated_tokens: u32,
    /// Server timings, when known.
    #[serde(default)]
    pub prefill_secs: f64,
    #[serde(default)]
    pub decode_secs: f64,
    /// The session node holding this reply's state: continue from it.
    #[serde(default)]
    pub session_node: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageRequest {
    pub model: String,
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    /// Reference images (encoded image bytes).
    #[serde(skip)]
    pub refs: Vec<Vec<u8>>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// Adapters to blend in (the story's picks for this model).
    #[serde(default)]
    pub loras: Vec<crate::model::Lora>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoRequest {
    pub model: String,
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    pub seconds: f32,
    /// First frame / reference images (encoded image bytes).
    #[serde(skip)]
    pub refs: Vec<Vec<u8>>,
    pub audio: bool,
    /// Reference audio clips (encoded audio bytes): the voices of those who speak in it.
    /// Named in the prompt `<Audio 1>`, ... (or as the model says).
    #[serde(skip)]
    pub ref_audios: Vec<Vec<u8>>,
    #[serde(default)]
    pub seed: Option<u64>,
    /// Adapters to blend in (the story's picks for this model).
    #[serde(default)]
    pub loras: Vec<crate::model::Lora>,
}

/// Speech or a sound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioRequest {
    pub model: String,
    /// Speech: what's said. A sound: the sound, in words.
    pub text: String,
    /// Speech from a description: the voice and how it's said, in words.
    #[serde(default)]
    pub instruct: Option<String>,
    /// Speech in a clip's voice: the clip (encoded audio bytes) and what it says.
    #[serde(skip)]
    pub voice: Option<Vec<u8>>,
    #[serde(default)]
    pub voice_words: Option<String>,
    /// A sound's length (seconds) and whether it loops seamlessly.
    #[serde(default)]
    pub seconds: Option<f32>,
    #[serde(default)]
    pub looped: bool,
    #[serde(default)]
    pub seed: Option<u64>,
}

/// An adapter the vault holds for a model, for picking (its name is the vault's, shown
/// only to whoever has the password).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Adapter {
    pub id: String,
    pub name: String,
    /// The weight suggested when it was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Text(TextRequest),
    Image(ImageRequest),
    Video(VideoRequest),
    Audio(AudioRequest),
}

impl Job {
    pub fn kind(&self) -> JobKind {
        match self {
            Job::Text(_) => JobKind::Text,
            Job::Image(_) => JobKind::Image,
            Job::Video(_) => JobKind::Video,
            Job::Audio(_) => JobKind::Audio,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Text,
    Image,
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobOutput {
    Text(TextResponse),
    /// Encoded bytes and their MIME type.
    Media {
        bytes: Vec<u8>,
        mime: String,
    },
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum BackendError {
    #[error("cancelled")]
    Cancelled,
    /// The session or node is gone (evicted, expired, server restarted): resend the
    /// whole conversation as a new root.
    #[error("session state gone")]
    SessionGone,
    #[error("{0}")]
    Failed(String),
}

pub type JobId = String;

/// A submitted job.
pub struct Submitted {
    pub id: JobId,
    pub done: BoxFuture<'static, Result<JobOutput, BackendError>>,
    /// A text job's reply as it is generated (pieces, in order), when the backend streams.
    pub text: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
}

/// Queue state as the backend sees it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: JobId,
    pub kind: Option<JobKind>,
    pub priority: Priority,
    pub running: bool,
    /// 0..1 when known.
    pub progress: Option<f32>,
    /// What the engine wants it for (filled in by the engine, not the backend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what: Option<String>,
    /// What it's for in the reader's terms ("Picture for the next screen"), relative to
    /// where they are (filled in by the engine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Seconds since the engine sent it (filled in by the engine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<f64>,
    /// Not sent yet: lined up for a free slot (no backend job; `id` is empty).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lined: bool,
}

/// What a model can do, as the backend reports it (thinfer's `GET /models`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelCaps {
    pub id: String,
    /// None: a kind yarner doesn't use (face swap, ...).
    pub kind: Option<JobKind>,
    pub title: String,
    /// The backend's default model of its kind.
    pub default: bool,
    /// Its weights are already on the server.
    pub downloaded: bool,
    /// Its jobs can pause and resume (a waiting reader's text can go first).
    pub pausable: bool,
    /// Reference images it takes (0: none) and how a prompt names the n-th
    /// (`{n}` from 1), when it can.
    pub max_refs: u32,
    pub ref_tag: Option<String>,
    /// Width and height must be multiples of this; its default size.
    pub size_multiple: u32,
    pub default_size: Option<(u32, u32)>,
    /// Reference audio clips it takes (0: none; a video model: voices to speak in) and
    /// how a prompt names the n-th.
    #[serde(default)]
    pub max_ref_audios: u32,
    #[serde(default)]
    pub audio_tag: Option<String>,
    /// What it makes includes sound (a video with a soundtrack; an audio model).
    #[serde(default)]
    pub makes_audio: bool,
    /// An audio model: what it makes and takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioCaps>,
}

/// What an audio model makes and takes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioCaps {
    /// Speech (says the text) rather than a sound (makes what the text describes).
    pub speech: bool,
    /// Takes a written voice / delivery description.
    pub instruct: bool,
    /// Speaks in a reference clip's voice (needs one).
    pub clone: bool,
    /// Longest clip one job makes (seconds).
    pub max_seconds: f32,
}

pub trait Backend: Send + Sync + 'static {
    /// `preempt`: run it next, pausing a running pausable job (which resumes after it
    /// with nothing lost), for a reader waiting on this job.
    fn submit(
        &self,
        job: Job,
        priority: Priority,
        preempt: bool,
    ) -> BoxFuture<'_, Result<Submitted, BackendError>>;
    fn reprioritize(&self, id: &JobId, priority: Priority) -> BoxFuture<'_, ()>;
    fn cancel(&self, id: &JobId) -> BoxFuture<'_, ()>;
    fn queue(&self) -> BoxFuture<'_, Vec<QueueEntry>>;
    /// Open a text session for `model`. Backends without sessions return `None`.
    fn create_session(&self, model: &str) -> BoxFuture<'_, Option<String>>;
    /// Forget a session.
    fn drop_session(&self, session: &str) -> BoxFuture<'_, ()>;
    /// The password of the backend's adapter vault (LoRAs), for every picture and video
    /// job from now on (`None`: none). Held in memory only.
    fn set_vault(&self, _password: Option<String>) {}
    /// Is a vault password set?
    fn vault_set(&self) -> bool {
        false
    }
    /// Does the vault open with the password set? `None`: can't tell (no vault, no
    /// password, the generator didn't answer).
    fn vault_check(&self) -> BoxFuture<'_, Option<bool>> {
        Box::pin(async { None })
    }
    /// The adapters the vault holds for `model`. `None`: no vault password set, or the
    /// generator has no vault.
    fn adapters(&self, model: String) -> BoxFuture<'_, Option<Vec<Adapter>>> {
        let _ = model;
        Box::pin(async { None })
    }
    /// Has a worker taken the job (it's being made, not just waiting its turn)? Backends
    /// that can't tell say yes.
    fn started(&self, _id: &JobId) -> bool {
        true
    }
    /// Where a job not yet started stands in the backend's line (1 = next), if it says.
    fn queued(&self, _id: &JobId) -> Option<u32> {
        None
    }
    /// Has a worker taken the job but is still loading the model it runs?
    fn loading(&self, _id: &JobId) -> bool {
        false
    }
    /// How far a running job is, 0..1, if the backend reports it.
    fn progress(&self, _id: &JobId) -> Option<f32> {
        None
    }
    /// The models the backend can run (empty: it doesn't say).
    fn models(&self) -> BoxFuture<'_, Vec<ModelCaps>> {
        Box::pin(async { vec![] })
    }
}
