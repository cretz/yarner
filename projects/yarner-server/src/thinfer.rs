//! The thinfer-serve backend, through thinfer's client crate. Every job is sealed under
//! a fresh per-job key before it leaves this process; results come back sealed and are
//! opened here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::json;
use thinfer_client::text_format::TextFormat;
use thinfer_client::tools as ttools;
use thinfer_client::wire::{
    AudioSpec, ImageSpec, JobSpec, JobStateKind, TextMessage, TextSpec, VideoSpec,
};
use thinfer_client::{Client, JobKey, SubmitOptions};
use yarner_engine::backend::*;

pub struct ThinferBackend {
    client: Client,
    /// Our live jobs (thinfer-serve lists everyone's; we only touch ours).
    jobs: Mutex<HashMap<JobId, Arc<thinfer_client::Job>>>,
    kinds: Mutex<HashMap<JobId, JobKind>>,
    /// Open text sessions by id (each holds its key; the server never keeps it).
    sessions: Mutex<HashMap<String, Arc<thinfer_client::Session>>>,
    /// How far each running picture/video job is (from its progress stream).
    progress: Arc<Mutex<HashMap<JobId, f32>>>,
    /// Jobs a worker has taken (not just queued).
    started: Arc<Mutex<std::collections::HashSet<JobId>>>,
    /// Jobs whose worker is still loading the model.
    loading: Arc<Mutex<std::collections::HashSet<JobId>>>,
    /// Where queued jobs stand in line (from their progress streams).
    line: Arc<Mutex<HashMap<JobId, u32>>>,
    /// What each model needs width and height to be multiples of.
    multiples: Mutex<HashMap<String, u32>>,
    /// For the vault's own endpoints (the client crate has none): base URL and auth.
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    /// The adapter vault's password (memory only).
    vault: Mutex<Option<zeroize::Zeroizing<String>>>,
}

pub struct Options {
    pub token: Option<String>,
    /// PEM of a CA to trust (thinfer-serve's self-signed certificate).
    pub ca_pem: Option<Vec<u8>>,
}

impl ThinferBackend {
    pub fn new(url: &str, opts: Options) -> anyhow::Result<Self> {
        let mut b = Client::builder(url);
        if let Some(t) = opts.token.clone() {
            b = b.token(t);
        }
        if let Some(pem) = &opts.ca_pem {
            b = b.root_certificate_pem(pem);
        }
        let mut http = reqwest::Client::builder();
        if let Some(pem) = &opts.ca_pem {
            http = http.add_root_certificate(reqwest::Certificate::from_pem(pem)?);
        }
        Ok(Self {
            http: http.build()?,
            base: url.trim_end_matches('/').to_string(),
            token: opts.token.clone(),
            vault: Mutex::new(None),
            client: b
                .build()
                .map_err(|e| anyhow::anyhow!("thinfer client: {e}"))?,
            jobs: Mutex::new(HashMap::new()),
            kinds: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            progress: Arc::new(Mutex::new(HashMap::new())),
            started: Arc::new(Mutex::new(std::collections::HashSet::new())),
            loading: Arc::new(Mutex::new(std::collections::HashSet::new())),
            line: Arc::new(Mutex::new(HashMap::new())),
            multiples: Mutex::new(HashMap::new()),
        })
    }
}

fn model<T: serde::de::DeserializeOwned>(name: &str) -> Result<T, BackendError> {
    serde_json::from_value(json!(name))
        .map_err(|_| BackendError::Failed(format!("thinfer doesn't know the model {name:?}")))
}

/// How many reference images a picture model takes.
/// Reference images a picture model takes (shared with the engine, which names them).
fn image_refs(model: &str) -> usize {
    yarner_engine::engine::refs_accepted(model)
}

impl ThinferBackend {
    /// The adapters the vault holds for `model`, by the password set. `None`: no password
    /// set, a refused one, or no answer.
    async fn adapter_list(&self, model: &str) -> Option<Vec<Adapter>> {
        let pw = self
            .vault
            .lock()
            .expect("vault")
            .as_ref()
            .map(|p| p.to_string())?;
        let mut req = self
            .http
            .post(format!("{}/vault/adapters/list", self.base))
            .timeout(std::time::Duration::from_secs(10))
            .json(&json!({ "model": model, "password": pw }));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let r = req.send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        let v = r.json::<serde_json::Value>().await.ok()?;
        Some(
            v["adapters"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|a| {
                    Some(Adapter {
                        id: a["id"].as_str()?.to_string(),
                        name: a["name"].as_str().unwrap_or("").to_string(),
                        // A number, or a number written as text.
                        weight: a["extra"]["weight"]
                            .as_f64()
                            .map(|w| w as f32)
                            .or_else(|| a["extra"]["weight"].as_str().and_then(|w| w.parse().ok())),
                    })
                })
                .collect(),
        )
    }
}

/// A model as thinfer's wire names it ("qwen-image-2.1-turbo").
fn wire<T: serde::Serialize>(m: &T) -> String {
    serde_json::to_value(m)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn lora_specs(l: &[yarner_engine::model::Lora]) -> Vec<thinfer_client::wire::LoraSpec> {
    l.iter()
        .map(|x| thinfer_client::wire::LoraSpec {
            id: x.id.clone(),
            weight: Some(x.weight as f32),
        })
        .collect()
}

/// `multiple`: what the job's model needs width and height to be multiples of.
fn spec(
    job: Job,
    session: Option<&thinfer_client::Session>,
    multiple: &dyn Fn(&str) -> u32,
) -> Result<(JobSpec, JobKind), BackendError> {
    Ok(match job {
        Job::Text(t) => {
            // A tool turn names the call it answers, and thinfer wants that call's tool
            // name too (2026-11-21 alpha: without it every lookup failed the beat).
            let names: std::collections::HashMap<String, String> = t
                .messages
                .iter()
                .flat_map(|m| m.tool_calls.iter())
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
            let messages = t
                .messages
                .into_iter()
                .map(|m| {
                    let mut tm = TextMessage::new(&m.role, m.content);
                    tm.name = m.name.clone().or_else(|| {
                        m.tool_call_id
                            .as_ref()
                            .and_then(|id| names.get(id).cloned())
                    });
                    tm.tool_calls = m
                        .tool_calls
                        .into_iter()
                        .map(|c| ttools::ToolCall {
                            id: c.id,
                            name: c.name,
                            arguments: c.arguments,
                        })
                        .collect();
                    tm.tool_call_id = m.tool_call_id;
                    tm
                })
                .collect();
            let mut s = TextSpec::new(messages);
            s.model = Some(model(&t.model)?);
            s.max_tokens = t.max_tokens;
            s.temperature = t.temperature;
            s.seed = t.seed;
            s.think = Some(t.think);
            s.format = t.schema.map(|schema| TextFormat::JsonSchema { schema });
            if let (Some(r), Some(sess)) = (&t.session, session) {
                s.session = Some(sess.at(r.from.as_deref()));
            }
            s.tools = t
                .tools
                .into_iter()
                .map(|d| ttools::ToolDef {
                    name: d.name,
                    description: d.description,
                    parameters: d.parameters,
                })
                .collect();
            (JobSpec::Text(s), JobKind::Text)
        }
        Job::Image(i) => {
            let mut s = ImageSpec::new(i.prompt);
            let max = image_refs(&i.model);
            s.model = Some(model(&i.model)?);
            let m = multiple(&i.model);
            s.width = Some(dim(i.width, m));
            s.height = Some(dim(i.height, m));
            s.seed = i.seed;
            s.input_images = i.refs.iter().take(max).map(|r| B64.encode(r)).collect();
            s.lora = lora_specs(&i.loras);
            // The story's shape, whatever the references' (qwen-image-2.1 otherwise takes
            // the last reference's aspect: square pictures in a portrait story).
            s.keep_size = Some(true);
            (JobSpec::Image(s), JobKind::Image)
        }
        Job::Video(v) => {
            let omni = v.model == "fasth3-omni";
            let refs: Vec<String> = if omni {
                v.refs.iter().take(9).map(|r| B64.encode(r)).collect()
            } else {
                vec![]
            };
            // FastH3-Omni reads its references by name: the LLM's prompt names them
            // itself (and says how to use them); if it didn't, name them so they count.
            let prompt = if refs.is_empty() || v.prompt.contains("<Picture ") {
                v.prompt
            } else {
                let names: Vec<String> =
                    (1..=refs.len()).map(|i| format!("<Picture {i}>")).collect();
                format!("{} (as in {})", v.prompt, names.join(", "))
            };
            let mut s = VideoSpec::new(prompt);
            s.model = Some(model(&v.model)?);
            let m = multiple(&v.model);
            s.width = Some(dim(v.width, m));
            s.height = Some(dim(v.height, m));
            s.durations = Some(vec![v.seconds]);
            s.seed = v.seed;
            s.audio = Some(v.audio);
            // Voices to speak in: only alongside reference pictures (thinfer's rule).
            if omni && !refs.is_empty() && !v.ref_audios.is_empty() {
                s.ref_audios = Some(v.ref_audios.iter().take(3).map(|a| B64.encode(a)).collect());
            }
            s.ref_images = (!refs.is_empty()).then_some(refs);
            s.lora = lora_specs(&v.loras);
            (JobSpec::Video(s), JobKind::Video)
        }
        Job::Audio(a) => {
            let s = AudioSpec {
                model: Some(model(&a.model)?),
                text: a.text,
                instruct: a.instruct,
                ref_audios: a.voice.map(|v| vec![B64.encode(v)]),
                ref_text: a.voice_words,
                seed: a.seed,
                // A sound's own settings (a speech model refuses them).
                seconds: a.seconds,
                looped: a.looped.then_some(true),
                ..Default::default()
            };
            (JobSpec::Audio(s), JobKind::Audio)
        }
    })
}

/// What yarner needs to know about one of thinfer's models.
fn caps(m: thinfer_client::wire::ModelInfo) -> yarner_engine::backend::ModelCaps {
    use thinfer_client::wire::{AudioKindInfo, InputUse, Media, ModelKind};
    let kind = match m.kind {
        ModelKind::Text => Some(JobKind::Text),
        ModelKind::Image => Some(JobKind::Image),
        ModelKind::Video => Some(JobKind::Video),
        ModelKind::Audio => Some(JobKind::Audio),
        _ => None,
    };
    // References: images the output draws on. An edit model's one input counts too
    // ("this character, now on the bridge" is what edit models do well); a face to swap
    // in doesn't.
    let refs = m.inputs.iter().find(|i| {
        i.media == Media::Image
            && matches!(
                i.used_as,
                InputUse::Condition | InputUse::Reference | InputUse::FirstFrame | InputUse::Edit
            )
    });
    // Reference audio a video draws on (voices to speak in).
    let audio_refs = m
        .inputs
        .iter()
        .find(|i| i.media == Media::Audio && i.used_as == InputUse::Reference);
    let clone = m.inputs.iter().any(|i| i.used_as == InputUse::Voice);
    let audio = m.audio.as_ref().map(|a| yarner_engine::backend::AudioCaps {
        speech: a.kind == AudioKindInfo::Speech,
        instruct: a.instruct,
        clone,
        max_seconds: a.max_seconds,
    });
    yarner_engine::backend::ModelCaps {
        max_ref_audios: audio_refs.map_or(0, |r| r.max),
        audio_tag: audio_refs.and_then(|r| r.prompt_tag.clone()),
        makes_audio: m.outputs.contains(&Media::Audio),
        audio,
        id: m.id,
        kind,
        title: m.title,
        default: m.default,
        downloaded: m.downloaded,
        pausable: m.pausable,
        max_refs: refs.map_or(0, |r| r.max),
        ref_tag: refs.and_then(|r| r.prompt_tag.clone()),
        size_multiple: m.size.as_ref().map_or(16, |s| s.multiple),
        default_size: m.size.as_ref().map(|s| (s.default_width, s.default_height)),
    }
}

/// A progress report as a share of the whole job (denoising is most of it; decoding the
/// result is the last few percent).
fn fraction(s: &thinfer_client::progress::Stage) -> Option<f32> {
    use thinfer_client::progress::Stage;
    let share = |done: u32, of: u32| (of > 0).then(|| done.min(of) as f32 / of as f32);
    Some(match *s {
        Stage::TextEncode => 0.02,
        Stage::Step { i, n } => 0.02 + 0.9 * share(i, n)?,
        Stage::ChunkStep {
            chunk,
            num_chunks,
            step,
            num_steps,
        } => {
            let done = chunk.saturating_sub(1) * num_steps + step;
            0.02 + 0.9 * share(done, num_chunks * num_steps)?
        }
        Stage::VaeDecode => 0.94,
        Stage::FrameSwapped { done, total } => 0.94 + 0.05 * share(done, total)?,
        #[allow(unreachable_patterns)]
        _ => return None,
    })
}

/// Round a size down to a multiple the model takes (a sane minimum kept).
fn dim(x: u32, multiple: u32) -> u32 {
    let m = multiple.max(1);
    (x / m * m).max(4 * m)
}

fn failed(e: impl std::fmt::Display) -> BackendError {
    let m = e.to_string();
    // A job we cancelled (thinfer_client::Error::Cancelled): not a failure, it's redone
    // later. Treated as one, a cancelled picture was marked failed and never painted.
    if m == "job was cancelled" {
        return BackendError::Cancelled;
    }
    // thinfer reports evicted / expired session state like this; the caller resends.
    if m.contains("gone") || m.contains("unknown session") || m.contains("unknown node") {
        return BackendError::SessionGone;
    }
    BackendError::Failed(m)
}

impl Backend for ThinferBackend {
    fn submit(
        &self,
        job: Job,
        priority: Priority,
        preempt: bool,
    ) -> BoxFuture<'_, Result<Submitted, BackendError>> {
        Box::pin(async move {
            let session = match &job {
                Job::Text(TextRequest {
                    session: Some(r), ..
                }) => Some(
                    self.sessions
                        .lock()
                        .expect("sessions")
                        .get(&r.session)
                        .cloned()
                        .ok_or(BackendError::SessionGone)?,
                ),
                _ => None,
            };
            // Each model's size rule (from its model list; fetched once): a size the
            // story chose that the model can't take is rounded down, never refused.
            if self.multiples.lock().expect("multiples").is_empty()
                && let Ok(list) = self.client.models().await
            {
                *self.multiples.lock().expect("multiples") = list
                    .models
                    .into_iter()
                    .map(|m| (m.id, m.size.map_or(16, |s| s.multiple)))
                    .collect();
            }
            let multiples = self.multiples.lock().expect("multiples").clone();
            let (spec, kind) = spec(job, session.as_deref(), &|model| {
                multiples.get(model).copied().unwrap_or(16)
            })?;
            // The story's adapters, opened with the vault password. None set: the plain
            // model (a picture without its adapters beats no picture).
            let mut spec = spec;
            let pw = self
                .vault
                .lock()
                .expect("vault")
                .as_ref()
                .map(|p| p.to_string());
            let media = match &mut spec {
                JobSpec::Image(s) => Some((&mut s.lora, &mut s.password, wire(&s.model))),
                JobSpec::Video(s) => Some((&mut s.lora, &mut s.password, wire(&s.model))),
                _ => None,
            };
            if let Some((lora, password, model)) = media
                && !lora.is_empty()
            {
                // Only adapters the vault still has (one removed in thinfer since it was
                // picked would fail the picture).
                let held = match &pw {
                    Some(_) => self.adapter_list(&model).await,
                    None => None,
                };
                match held {
                    Some(list) => {
                        lora.retain(|l| list.iter().any(|a| a.id == l.id));
                        *password = pw.clone().filter(|_| !lora.is_empty());
                    }
                    None => lora.clear(),
                }
            }
            let opts = SubmitOptions {
                priority,
                preempt: preempt.then_some(thinfer_client::wire::PauseMode::StepEnd),
            };
            let j = Arc::new(
                self.client
                    .submit(&spec, JobKey::generate(), &opts)
                    .await
                    .map_err(failed)?,
            );
            let id = j.id.clone();
            self.jobs
                .lock()
                .expect("jobs")
                .insert(id.clone(), j.clone());
            self.kinds.lock().expect("kinds").insert(id.clone(), kind);
            let client = self.client.clone();
            let progress = self.progress.clone();
            let started = self.started.clone();
            let loading = self.loading.clone();
            let line = self.line.clone();
            let pid = id.clone();
            let (stream, text) = if kind == JobKind::Text {
                let (s, r) = tokio::sync::mpsc::unbounded_channel();
                (Some(s), Some(r))
            } else {
                (None, None)
            };
            let done = Box::pin(async move {
                let finished = client
                    .follow(&j, |u| match u {
                        thinfer_client::Update::Queued { position } => {
                            line.lock()
                                .expect("line")
                                .insert(pid.clone(), position as u32 + 1);
                        }
                        thinfer_client::Update::Started => {
                            started.lock().expect("started").insert(pid.clone());
                            line.lock().expect("line").remove(&pid);
                        }
                        thinfer_client::Update::Text(t) => {
                            started.lock().expect("started").insert(pid.clone());
                            loading.lock().expect("loading").remove(&pid);
                            if let Some(s) = &stream {
                                let _ = s.send(t.to_string());
                            }
                        }
                        thinfer_client::Update::Progress(stage) => {
                            started.lock().expect("started").insert(pid.clone());
                            let mut l = loading.lock().expect("loading");
                            if matches!(stage, thinfer_client::progress::Stage::Loading) {
                                l.insert(pid.clone());
                            } else {
                                l.remove(&pid);
                            }
                            drop(l);
                            if let Some(f) = fraction(&stage) {
                                progress.lock().expect("progress").insert(pid.clone(), f);
                            }
                        }
                        _ => {}
                    })
                    .await;
                progress.lock().expect("progress").remove(&pid);
                started.lock().expect("started").remove(&pid);
                loading.lock().expect("loading").remove(&pid);
                line.lock().expect("line").remove(&pid);
                match finished {
                    Err(e) => Err(failed(e)),
                    Ok(f) => {
                        // Fetching is what deletes the job server-side.
                        let artifact = client.fetch_result(&j).await.map_err(failed)?;
                        match kind {
                            JobKind::Text => Ok(JobOutput::Text(TextResponse {
                                content: String::from_utf8_lossy(&artifact).into_owned(),
                                // thinfer wipes its copies on drop; take clones.
                                tool_calls: f
                                    .tool_calls
                                    .iter()
                                    .map(|c| ToolCall {
                                        id: c.id.clone(),
                                        name: c.name.clone(),
                                        arguments: c.arguments.clone(),
                                    })
                                    .collect(),
                                prompt_tokens: f
                                    .result
                                    .text
                                    .as_ref()
                                    .map_or(0, |t| t.prompt_tokens),
                                generated_tokens: f
                                    .result
                                    .text
                                    .as_ref()
                                    .map_or(0, |t| t.generated_tokens),
                                prefill_secs: f
                                    .result
                                    .text
                                    .as_ref()
                                    .map_or(0.0, |t| t.prefill_secs),
                                decode_secs: f.result.text.as_ref().map_or(0.0, |t| t.decode_secs),
                                session_node: f
                                    .result
                                    .text
                                    .as_ref()
                                    .and_then(|t| t.session_node.clone()),
                            })),
                            JobKind::Image => Ok(JobOutput::Media {
                                bytes: artifact.to_vec(),
                                mime: "image/png".into(),
                            }),
                            JobKind::Video => Ok(JobOutput::Media {
                                bytes: artifact.to_vec(),
                                mime: "video/mp4".into(),
                            }),
                            // A mono 16-bit WAV.
                            JobKind::Audio => Ok(JobOutput::Media {
                                bytes: artifact.to_vec(),
                                mime: "audio/wav".into(),
                            }),
                        }
                    }
                }
            });
            Ok(Submitted { id, done, text })
        })
    }

    fn reprioritize(&self, id: &JobId, priority: Priority) -> BoxFuture<'_, ()> {
        let j = self.jobs.lock().expect("jobs").get(id).cloned();
        Box::pin(async move {
            if let Some(j) = j {
                // A job already running can't change; that's fine.
                let _ = self.client.set_priority(&j, priority).await;
            }
        })
    }

    fn cancel(&self, id: &JobId) -> BoxFuture<'_, ()> {
        let j = self.jobs.lock().expect("jobs").remove(id);
        Box::pin(async move {
            if let Some(j) = j {
                let _ = self.client.cancel(&j).await;
            }
        })
    }

    fn queue(&self) -> BoxFuture<'_, Vec<QueueEntry>> {
        Box::pin(async move {
            let Ok(list) = self.client.list_jobs().await else {
                return vec![];
            };
            let ours = self.kinds.lock().expect("kinds").clone();
            let mut live: Vec<JobId> = Vec::new();
            let out = list
                .jobs
                .into_iter()
                .filter(|e| ours.contains_key(&e.id))
                .filter(|e| {
                    matches!(
                        e.state,
                        JobStateKind::Queued | JobStateKind::Starting | JobStateKind::Running
                    )
                })
                .map(|e| {
                    live.push(e.id.clone());
                    QueueEntry {
                        kind: ours.get(&e.id).copied(),
                        running: !matches!(e.state, JobStateKind::Queued),
                        priority: e.priority,
                        progress: self.progress.lock().expect("progress").get(&e.id).copied(),
                        what: None,
                        id: e.id,
                        ..Default::default()
                    }
                })
                .collect();
            // Forget jobs the server no longer reports as live.
            let mut kinds = self.kinds.lock().expect("kinds");
            kinds.retain(|id, _| live.contains(id));
            self.jobs
                .lock()
                .expect("jobs")
                .retain(|id, _| live.contains(id));
            out
        })
    }

    fn models(&self) -> BoxFuture<'_, Vec<yarner_engine::backend::ModelCaps>> {
        Box::pin(async move {
            let Ok(list) = self.client.models().await else {
                return vec![];
            };
            list.models.into_iter().map(caps).collect()
        })
    }

    fn progress(&self, id: &JobId) -> Option<f32> {
        self.progress.lock().expect("progress").get(id).copied()
    }

    fn loading(&self, id: &JobId) -> bool {
        self.loading.lock().expect("loading").contains(id)
    }
    fn started(&self, id: &JobId) -> bool {
        self.started.lock().expect("started").contains(id)
    }

    fn queued(&self, id: &JobId) -> Option<u32> {
        self.line.lock().expect("line").get(id).copied()
    }

    fn set_vault(&self, password: Option<String>) {
        *self.vault.lock().expect("vault") = password
            .filter(|p| !p.is_empty())
            .map(zeroize::Zeroizing::new);
    }

    fn adapters(&self, model: String) -> BoxFuture<'_, Option<Vec<Adapter>>> {
        Box::pin(async move { self.adapter_list(&model).await })
    }

    fn vault_check(&self) -> BoxFuture<'_, Option<bool>> {
        Box::pin(async move {
            let pw = self
                .vault
                .lock()
                .expect("vault")
                .as_ref()
                .map(|p| p.to_string())?;
            // Any picture or video model: a wrong password is refused whatever it names.
            let model = self
                .models()
                .await
                .into_iter()
                .find(|m| {
                    matches!(
                        m.kind,
                        Some(yarner_engine::backend::JobKind::Image)
                            | Some(yarner_engine::backend::JobKind::Video)
                    )
                })?
                .id;
            let mut req = self
                .http
                .post(format!("{}/vault/adapters/list", self.base))
                .timeout(std::time::Duration::from_secs(10))
                .json(&json!({ "model": model, "password": pw }));
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            match req.send().await.ok()?.status() {
                s if s.is_success() => Some(true),
                reqwest::StatusCode::UNAUTHORIZED => Some(false),
                _ => None,
            }
        })
    }

    fn vault_set(&self) -> bool {
        self.vault.lock().expect("vault").is_some()
    }

    fn create_session(&self, name: &str) -> BoxFuture<'_, Option<String>> {
        let name = name.to_string();
        Box::pin(async move {
            let m = model::<thinfer_client::model::TextModelId>(&name).ok()?;
            // An hour idle; the engine keeps using it while the story is open.
            let s = self.client.create_session(Some(m), Some(3600)).await.ok()?;
            let id = s.id.clone();
            self.sessions
                .lock()
                .expect("sessions")
                .insert(id.clone(), Arc::new(s));
            Some(id)
        })
    }

    fn drop_session(&self, session: &str) -> BoxFuture<'_, ()> {
        let s = self.sessions.lock().expect("sessions").remove(session);
        Box::pin(async move {
            if let Some(s) = s {
                let _ = self.client.delete_session(&s).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool result goes back with the id of the call it answers and that call's tool
    /// name, as thinfer requires (it refuses the job otherwise).
    #[test]
    fn a_tool_result_names_its_call_and_tool() {
        use yarner_engine::backend::{Message, TextRequest, ToolCall};
        let mut asked = Message::new("assistant", "");
        asked.tool_calls = vec![ToolCall {
            id: "call7".into(),
            name: "search_images".into(),
            arguments: serde_json::json!({"query": "pier"}),
        }];
        let mut answer = Message::new("tool", "[]");
        answer.tool_call_id = Some("call7".into());
        let job = Job::Text(TextRequest {
            model: "gemma-4-26b-a4b".into(),
            messages: vec![Message::new("user", "Write the next beat."), asked, answer],
            ..Default::default()
        });
        let (spec, _) = spec(job, None, &|_| 16).unwrap();
        let wire = serde_json::to_value(&spec).unwrap();
        let msgs = wire
            .pointer("/messages")
            .or_else(|| wire.pointer("/text/messages"));
        let msgs = msgs
            .and_then(|m| m.as_array())
            .expect("messages on the wire");
        let tool = msgs
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("tool turn");
        assert_eq!(tool["toolCallId"], "call7", "{tool}");
        assert_eq!(tool["name"], "search_images", "{tool}");
        // A session continuing from the call sends only the result: it names its tool
        // itself (2026-11-21 alpha: the first fix missed this, every lookup still failed).
        let mut alone = Message::new("tool", "[]");
        alone.tool_call_id = Some("call8".into());
        alone.name = Some("recall".into());
        let job = Job::Text(TextRequest {
            model: "gemma-4-26b-a4b".into(),
            messages: vec![alone],
            ..Default::default()
        });
        let (sent, _) = super::spec(job, None, &|_| 16).unwrap();
        let wire = serde_json::to_value(&sent).unwrap();
        let msgs = wire
            .pointer("/messages")
            .or_else(|| wire.pointer("/text/messages"));
        let tool = &msgs.and_then(|m| m.as_array()).expect("messages")[0];
        assert_eq!(tool["name"], "recall", "{tool}");
    }

    #[test]
    fn progress_stages_become_a_rising_share() {
        use thinfer_client::progress::Stage;
        let f = |s| fraction(&s).unwrap();
        let steps: Vec<f32> = (1..=8).map(|i| f(Stage::Step { i, n: 8 })).collect();
        assert!(steps.windows(2).all(|w| w[0] < w[1]), "{steps:?}");
        assert!(f(Stage::TextEncode) < steps[0]);
        assert!(f(Stage::VaeDecode) > steps[7] && f(Stage::VaeDecode) < 1.0);
        assert_eq!(fraction(&Stage::Step { i: 1, n: 0 }), None);
    }

    #[test]
    fn sizes_round_to_the_models_multiple() {
        // The alpha's failure: 432 x 768 for a model that wants multiples of 32.
        assert_eq!((dim(432, 32), dim(768, 32)), (416, 768));
        assert_eq!(dim(832, 16), 832);
        assert_eq!(dim(10, 32), 128);
    }

    #[test]
    fn a_cancelled_job_is_not_a_failure() {
        assert!(matches!(
            failed(thinfer_client::Error::Cancelled),
            BackendError::Cancelled
        ));
        assert!(matches!(failed("out of memory"), BackendError::Failed(_)));
    }
}
