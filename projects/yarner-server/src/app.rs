//! The HTTP API.
//!
//! Unlocking a story returns a handle: a random capability naming that open story. Every
//! story route lives under `/api/s/{handle}`. Open stories hold their key in memory until
//! locked by hand or by the idle reaper (no client connected and idle past the timeout,
//! unless "keep generating" is on).
//!
//! Every response is `Cache-Control: no-store`: nothing decrypted may land in a browser
//! cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use yarner_engine::backend::Backend;
use yarner_engine::model::{Anchor, RefImage, Scope, Settings};
use yarner_engine::{Config, Engine, EngineError};
use yarner_format::Install;

pub struct Open {
    pub engine: Engine,
    pub title: String,
    pub last_activity: Instant,
    pub clients: usize,
    pub keep_until: Option<Instant>,
}

pub struct AppState {
    pub install: Arc<Install>,
    pub backend: Arc<dyn Backend>,
    pub engine_cfg: Config,
    pub idle_timeout: Duration,
    pub open: Mutex<HashMap<String, Open>>,
    /// A generator (thinfer or the mock) is connected. Without one, everything already
    /// written plays as usual; nothing new can be made.
    pub generator: bool,
    pub gpu: Gpu,
}

/// "Pause": the GPU given back. Every story stops and cancels its work (made again on
/// resume), then the private thinfer-serve is stopped (its memory freed) until resumed.
/// Kept across restarts by a file in the data folder.
pub struct Gpu {
    /// True = held; the supervisor of a private thinfer-serve follows it.
    pub hold: tokio::sync::watch::Sender<bool>,
    /// The private thinfer-serve is up (none: a backend that's always there).
    pub up: Option<tokio::sync::watch::Receiver<bool>>,
    /// Exists while paused.
    pub file: std::path::PathBuf,
    /// "running", "pausing", "paused" or "resuming".
    pub state: Mutex<&'static str>,
}

impl Gpu {
    pub fn new(file: std::path::PathBuf) -> (Self, tokio::sync::watch::Receiver<bool>) {
        let held = file.exists();
        let (hold, rx) = tokio::sync::watch::channel(held);
        let gpu = Gpu {
            hold,
            up: None,
            file,
            state: Mutex::new(if held { "paused" } else { "running" }),
        };
        (gpu, rx)
    }

    pub fn held(&self) -> bool {
        *self.hold.borrow()
    }
}

pub type Shared = Arc<AppState>;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        let code = match e {
            EngineError::NotFound(_) => StatusCode::NOT_FOUND,
            EngineError::Invalid(_) => StatusCode::BAD_REQUEST,
            EngineError::Format(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(code, e.to_string())
    }
}

impl From<yarner_format::Error> for ApiError {
    fn from(e: yarner_format::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

type R<T> = Result<T, ApiError>;

fn handle() -> String {
    hex::encode(yarner_format::crypto::random_array::<32>())
}

#[derive(Serialize, Deserialize, Default)]
struct Meta {
    #[serde(default)]
    title: String,
}

fn meta_of(story: &yarner_format::Story) -> Meta {
    story
        .meta()
        .ok()
        .and_then(|m| serde_json::from_slice(&m).ok())
        .unwrap_or_default()
}

/// Look up an open story, marking activity.
fn engine(s: &Shared, h: &str) -> R<Engine> {
    let mut open = s.open.lock().expect("open stories");
    let o = open
        .get_mut(h)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "story locked".into()))?;
    o.last_activity = Instant::now();
    Ok(o.engine.clone())
}

/// With no generator connected, refuse what can only be done by generating (rewriting a
/// beat, a new version, painting a picture) with a clear message instead of waiting forever.
async fn generator_guard(
    State(s): State<Shared>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    const NEEDS: &[&str] = &["/rerun", "/fork", "/media/generate"];
    let path = req.uri().path();
    if !s.generator
        && req.method() == axum::http::Method::POST
        && NEEDS.iter().any(|r| path.ends_with(r))
    {
        return ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no generator is connected: start yarner with thinfer to write or paint".into(),
        )
        .into_response();
    }
    next.run(req).await
}

pub fn router(state: Shared) -> Router {
    let story = Router::new()
        .route("/", get(story_info))
        .route("/lock", post(lock))
        .route("/delete", post(delete_story))
        .route("/new-from-setup", post(new_from_setup))
        .route("/keep", post(keep))
        .route("/title", post(set_title))
        .route("/passphrase", post(change_passphrase))
        .route("/setup", get(setup))
        .route("/prompt", post(set_prompt))
        .route("/prompt/everywhere", post(everywhere))
        .route("/object", post(put_object))
        .route("/object/{id}/voice", post(make_voice))
        .route("/object/{id}/remove", post(remove_object))
        .route("/settings", post(set_settings))
        // Photos and voice clips run past the 2 MB default.
        .route(
            "/media",
            post(upload_media).layer(axum::extract::DefaultBodyLimit::max(64 << 20)),
        )
        .route("/media/generate", post(generate_media))
        .route("/media/{id}", get(media))
        .route("/plays", get(list_plays).post(new_play))
        .route("/plays/{p}", get(play).delete(delete_play))
        .route("/plays/{p}/advance", post(advance))
        .route("/plays/{p}/back", post(back))
        .route("/plays/{p}/rewind", post(rewind))
        .route("/plays/{p}/jump", post(jump))
        .route("/plays/{p}/rerun", post(rerun))
        .route("/plays/{p}/fork", post(fork))
        .route("/plays/{p}/version", post(version))
        .route("/plays/{p}/favor", post(favor))
        .route("/plays/{p}/keep-going", post(keep_going))
        .route("/plays/{p}/own-choice", post(own_choice))
        .route("/plays/{p}/save", post(save))
        .route("/saves", get(saves))
        .route("/saves/{id}/load", post(load))
        .route("/saves/{id}", delete(delete_save))
        .route("/map", get(map))
        .route("/prefs", get(get_prefs).post(set_prefs))
        .route("/queue", get(queue))
        .route("/export", post(export))
        .route("/export/report", get(export_report))
        .route("/ws", get(ws));
    let generator = state.generator;
    Router::new()
        .route(
            "/api/ping",
            get(move || async move { Json(json!({ "ok": true, "generator": generator })) }),
        )
        .route("/api/unlock", post(unlock))
        .route("/api/models", get(models))
        .route("/api/gpu", get(gpu_get).post(gpu_set))
        .route("/api/vault", get(vault_get).post(vault_set))
        .route("/api/vault/adapters", get(adapters))
        .route("/api/clear-all", post(clear_all))
        .route("/api/stories", post(create_story))
        .route("/api/import", post(import))
        .nest("/api/s/{h}", story)
        // API misses are JSON 404s, never the client's index.html.
        .route("/api/{*rest}", axum::routing::any(api_missing))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            generator_guard,
        ))
        .with_state(state)
}

/// Security headers for every response.
pub async fn headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'",
        ),
    );
    res
}

#[derive(Deserialize)]
struct Passphrase {
    passphrase: String,
}

#[derive(Serialize)]
struct StoryRef {
    handle: String,
    id: String,
    title: String,
}

fn open_story(s: &Shared, story: yarner_format::Story) -> R<StoryRef> {
    let id = story.id();
    let mut open = s.open.lock().expect("open stories");
    if let Some((h, o)) = open.iter().find(|(_, o)| o.engine.story().id() == id) {
        return Ok(StoryRef {
            handle: h.clone(),
            id,
            title: o.title.clone(),
        });
    }
    let title = meta_of(&story).title;
    let engine = Engine::open(story, s.backend.clone(), s.engine_cfg.clone())?;
    if !s.generator {
        // Nothing can be generated: the scheduler never starts work.
        engine.set_paused(true);
    }
    if s.gpu.held() {
        engine.hold(true);
    }
    let h = handle();
    open.insert(
        h.clone(),
        Open {
            engine,
            title: title.clone(),
            last_activity: Instant::now(),
            clients: 0,
            keep_until: None,
        },
    );
    Ok(StoryRef {
        handle: h,
        id,
        title,
    })
}

async fn unlock(State(s): State<Shared>, Json(b): Json<Passphrase>) -> R<Json<Value>> {
    let inst = s.install.clone();
    let stories = tokio::task::spawn_blocking(move || inst.unlock(&b.passphrase))
        .await
        .map_err(|e| bad(e.to_string()))??;
    let mut out = Vec::new();
    for st in stories {
        out.push(open_story(&s, st)?);
    }
    Ok(Json(json!({ "stories": out })))
}

#[derive(Deserialize)]
struct NewStory {
    title: String,
    passphrase: String,
}

async fn create_story(State(s): State<Shared>, Json(b): Json<NewStory>) -> R<Json<StoryRef>> {
    if b.passphrase.chars().count() < 8 {
        return Err(bad("use a passphrase of at least 8 characters"));
    }
    let inst = s.install.clone();
    let meta = serde_json::to_vec(&Meta { title: b.title }).expect("json");
    let story = tokio::task::spawn_blocking(move || inst.create_story(&b.passphrase, &meta))
        .await
        .map_err(|e| bad(e.to_string()))??;
    Ok(Json(open_story(&s, story)?))
}

/// A new, empty story that starts with this one's setup (prompts, objects and their
/// reference pictures, settings). This story is left as it is.
async fn new_from_setup(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<NewStory>,
) -> R<Json<StoryRef>> {
    if b.passphrase.chars().count() < 8 {
        return Err(bad("use a passphrase of at least 8 characters"));
    }
    let e = engine(&s, &h)?;
    let inst = s.install.clone();
    let meta = serde_json::to_vec(&Meta { title: b.title }).expect("json");
    let story = tokio::task::spawn_blocking(move || -> Result<_, ApiError> {
        let story = inst.create_story(&b.passphrase, &meta)?;
        if let Err(err) = e.copy_setup_to(&story) {
            let _ = story.delete();
            return Err(err.into());
        }
        Ok(story)
    })
    .await
    .map_err(|e| bad(e.to_string()))??;
    Ok(Json(open_story(&s, story)?))
}

/// Body: the bundle. Headers: `x-bundle-passphrase` opens it, `x-passphrase` is the
/// story's passphrase from now on.
async fn import(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> R<Json<StoryRef>> {
    let get = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| bad(format!("missing {k}")))
    };
    let (bp, p) = (get("x-bundle-passphrase")?, get("x-passphrase")?);
    let inst = s.install.clone();
    let story = tokio::task::spawn_blocking(move || {
        yarner_format::bundle::import(&inst, std::io::Cursor::new(body), &bp, &p)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
    .map_err(|e| match e {
        yarner_format::Error::Decrypt => bad("wrong bundle passphrase"),
        e => e.into(),
    })?;
    Ok(Json(open_story(&s, story)?))
}

async fn lock(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    let gone = s.open.lock().expect("open").remove(&h);
    if let Some(o) = gone {
        o.engine.shutdown().await;
    }
    Ok(Json(json!({})))
}

/// The story ended here but the reader wants more.
async fn keep_going(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
) -> R<Json<Value>> {
    if !s.generator {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no generator is connected to write more".into(),
        ));
    }
    let e = engine(&s, &h)?;
    e.keep_going(&p)?;
    let r = e.advance(&p, None)?;
    Ok(Json(json!({ "result": r, "view": e.play_view(&p)? })))
}

#[derive(Deserialize)]
struct OwnChoice {
    text: String,
}

/// The reader's own choice at a decision: added, then taken.
async fn own_choice(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<OwnChoice>,
) -> R<Json<Value>> {
    if !s.generator {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no generator is connected to write what happens".into(),
        ));
    }
    let e = engine(&s, &h)?;
    let exit = e.add_choice(&p, &b.text)?;
    let r = e.advance(&p, Some(exit))?;
    Ok(Json(json!({ "result": r, "view": e.play_view(&p)? })))
}

/// Delete the story for good: its encrypted files and its saves. The passphrase, typed
/// again, is the confirmation (and must open this very story).
async fn delete_story(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Passphrase>,
) -> R<Json<Value>> {
    let id = engine(&s, &h)?.story().id();
    let inst = s.install.clone();
    let pass = b.passphrase;
    let story = tokio::task::spawn_blocking(move || inst.unlock(&pass))
        .await
        .map_err(|e| bad(e.to_string()))??
        .into_iter()
        .find(|st| st.id() == id)
        .ok_or_else(|| bad("that passphrase doesn't open this story"))?;
    // Close it first (nothing writes to it while it goes).
    let gone = s.open.lock().expect("open").remove(&h);
    if let Some(o) = gone {
        o.engine.shutdown().await;
    }
    tokio::task::spawn_blocking(move || story.delete())
        .await
        .map_err(|e| bad(e.to_string()))??;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Keep {
    /// Keep generating for this many hours with no client (0 turns it off).
    hours: f64,
    #[serde(default)]
    depth: Option<usize>,
}

async fn keep(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Keep>,
) -> R<Json<Value>> {
    let mut open = s.open.lock().expect("open");
    let o = open
        .get_mut(&h)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "story locked".into()))?;
    // A block of up to 12 hours (start another when it's up).
    let hours = if b.hours.is_finite() {
        b.hours.clamp(0.0, 12.0)
    } else {
        0.0
    };
    o.keep_until = (hours > 0.0).then(|| Instant::now() + Duration::from_secs_f64(hours * 3600.0));
    o.engine.set_depth(b.depth.unwrap_or(if hours > 0.0 {
        4
    } else {
        s.engine_cfg.speculate_depth
    }));
    // The time is used: once everything near the reader is made, the story is filled
    // out wide and deep until it's up (it stopped after ~6 beats down one line before).
    o.engine.set_fill(hours > 0.0);
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct Title {
    title: String,
}

async fn set_title(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Title>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.story().set_meta(
        &serde_json::to_vec(&Meta {
            title: b.title.clone(),
        })
        .expect("json"),
    )?;
    if let Some(o) = s.open.lock().expect("open").get_mut(&h) {
        o.title = b.title;
    }
    Ok(Json(json!({})))
}

async fn change_passphrase(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Passphrase>,
) -> R<Json<Value>> {
    if b.passphrase.chars().count() < 8 {
        return Err(bad("use a passphrase of at least 8 characters"));
    }
    let e = engine(&s, &h)?;
    let inst = s.install.clone();
    tokio::task::spawn_blocking(move || e.story().change_passphrase(&inst, &b.passphrase))
        .await
        .map_err(|e| bad(e.to_string()))??;
    Ok(Json(json!({})))
}

#[derive(Deserialize, Default)]
struct PlayQ {
    play: Option<String>,
}

async fn story_info(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    let (title, keep) = {
        let open = s.open.lock().expect("open");
        let o = &open[&h];
        (
            o.title.clone(),
            o.keep_until
                .map(|k| k.saturating_duration_since(Instant::now()).as_secs()),
        )
    };
    Ok(Json(json!({
        "id": e.story().id(),
        "title": title,
        "plays": e.plays(),
        "saves": e.saves(),
        "prefs": e.prefs(),
        "keep_secs": keep,
        "started": e.with_graph(|g| g.root.is_some()),
    })))
}

async fn setup(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Query(q): Query<PlayQ>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    Ok(Json(json!(e.setup_view(q.play.as_deref()))))
}

#[derive(Deserialize)]
struct PromptBody {
    id: String,
    text: String,
    #[serde(default)]
    anchor: Anchor,
    #[serde(default = "from_here")]
    scope: Scope,
}

fn from_here() -> Scope {
    Scope::FromHere
}

async fn set_prompt(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<PromptBody>,
) -> R<Json<Value>> {
    let rev = engine(&s, &h)?.set_prompt(&b.id, &b.text, b.anchor, b.scope)?;
    Ok(Json(json!({ "rev": rev })))
}

#[derive(Deserialize)]
struct Rev {
    rev: String,
}

async fn everywhere(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Rev>,
) -> R<Json<Value>> {
    let rev = engine(&s, &h)?.make_everywhere(&b.rev)?;
    Ok(Json(json!({ "rev": rev })))
}

#[derive(Deserialize)]
struct ObjectBody {
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    refs: Vec<RefImage>,
    #[serde(default)]
    talks: bool,
    #[serde(default)]
    voice: Option<yarner_engine::model::Voice>,
    #[serde(default)]
    anchor: Anchor,
}

#[derive(Deserialize)]
struct VoiceBody {
    voice: yarner_engine::model::Voice,
    /// The deliveries to make clips of (by index).
    #[serde(default)]
    make: Vec<usize>,
    #[serde(default)]
    anchor: Anchor,
}

/// Save an object's voice and make clips of the deliveries asked for.
async fn make_voice(
    State(s): State<Shared>,
    Path((h, id)): Path<(String, String)>,
    Json(b): Json<VoiceBody>,
) -> R<Json<Value>> {
    let v = engine(&s, &h)?.make_voice(&id, b.voice, &b.make, b.anchor)?;
    Ok(Json(json!({ "voice": v })))
}

async fn put_object(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<ObjectBody>,
) -> R<Json<Value>> {
    if b.name.trim().is_empty() {
        return Err(bad("an object needs a name"));
    }
    let id = engine(&s, &h)?.put_object(
        b.id.as_deref(),
        b.name.trim(),
        &b.description,
        b.refs,
        b.talks,
        b.voice,
        b.anchor,
    )?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize, Default)]
struct AnchorBody {
    #[serde(default)]
    anchor: Anchor,
}

async fn remove_object(
    State(s): State<Shared>,
    Path((h, id)): Path<(String, String)>,
    Json(b): Json<AnchorBody>,
) -> R<Json<Value>> {
    engine(&s, &h)?.remove_object(&id, b.anchor)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct SettingsBody {
    settings: Settings,
    #[serde(default)]
    anchor: Anchor,
}

async fn set_settings(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<SettingsBody>,
) -> R<Json<Value>> {
    if b.settings.width < 64
        || b.settings.height < 64
        || b.settings.width > 4096
        || b.settings.height > 4096
        || b.settings
            .video_size
            .is_some_and(|(w, h)| !(64..=4096).contains(&w) || !(64..=4096).contains(&h))
    {
        return Err(bad("resolution out of range"));
    }
    engine(&s, &h)?.set_settings(b.settings, b.anchor)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct UploadQ {
    #[serde(default)]
    caption: String,
    #[serde(default)]
    objects: String,
}

async fn upload_media(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Query(q): Query<UploadQ>,
    headers: HeaderMap,
    body: Bytes,
) -> R<Json<Value>> {
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    if !(mime.starts_with("image/") || mime.starts_with("video/") || mime.starts_with("audio/")) {
        return Err(bad("upload an image, video or audio file"));
    }
    let objects = q
        .objects
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect();
    let id = engine(&s, &h)?.upload_media(&body, &mime, &q.caption, objects)?;
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
struct GenBody {
    prompt: String,
    #[serde(default)]
    caption: String,
    #[serde(default)]
    objects: Vec<String>,
    #[serde(default)]
    refs: Vec<String>,
}

async fn generate_media(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<GenBody>,
) -> R<Json<Value>> {
    let id = engine(&s, &h)?.request_image(&b.prompt, &b.caption, b.objects, b.refs)?;
    Ok(Json(json!({ "id": id })))
}

/// Decrypted media, with byte ranges for video seeking.
async fn media(
    State(s): State<Shared>,
    Path((h, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> R<Response> {
    let e = engine(&s, &h)?;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    tokio::task::spawn_blocking(move || -> R<Response> {
        let (mut reader, mime) = e.media_reader(&id)?;
        let len = reader.len();
        let parsed = range.as_deref().and_then(|r| parse_range(r, len));
        let (status, start, end) = match parsed {
            Some((a, b)) => (StatusCode::PARTIAL_CONTENT, a, b),
            None => (StatusCode::OK, 0, len),
        };
        let bytes = reader.range(start, end)?;
        let mut res = Response::new(Body::from(bytes.to_vec()));
        *res.status_mut() = status;
        let h = res.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&mime)
                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
        );
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if status == StatusCode::PARTIAL_CONTENT {
            h.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{}/{len}", end.saturating_sub(1)))
                    .expect("ascii"),
            );
        }
        Ok(res)
    })
    .await
    .map_err(|e| bad(e.to_string()))?
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` -> half-open range within `len`.
pub fn parse_range(r: &str, len: u64) -> Option<(u64, u64)> {
    let spec = r.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.is_empty(), b.is_empty()) {
        (true, false) => {
            let n: u64 = b.parse().ok()?;
            (len.saturating_sub(n), len)
        }
        (false, true) => (a.parse().ok()?, len),
        (false, false) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.saturating_add(1).min(len),
        ),
        (true, true) => return None,
    };
    (start < end && start < len).then_some((start, end))
}

async fn list_plays(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    Ok(Json(json!(engine(&s, &h)?.plays())))
}

#[derive(Deserialize)]
struct Name {
    #[serde(default)]
    name: String,
}

async fn new_play(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Name>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    let name = if b.name.trim().is_empty() {
        format!("Playthrough {}", e.plays().len() + 1)
    } else {
        b.name
    };
    let id = e.new_play(&name)?;
    e.activate(&id);
    Ok(Json(json!({ "id": id })))
}

async fn play(State(s): State<Shared>, Path((h, p)): Path<(String, String)>) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.activate(&p);
    Ok(Json(json!(e.play_view(&p)?)))
}

async fn delete_play(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
) -> R<Json<Value>> {
    engine(&s, &h)?.delete_play(&p)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize, Default)]
struct ExitBody {
    #[serde(default)]
    exit: Option<usize>,
}

async fn advance(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<ExitBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    let mut r = e.advance(&p, b.exit)?;
    // No generator: what isn't written already can't be now. This line ends here.
    if !s.generator && r == yarner_engine::Advance::Waiting {
        r = yarner_engine::Advance::Ended;
    }
    Ok(Json(json!({ "result": r, "view": e.play_view(&p)? })))
}

async fn back(State(s): State<Shared>, Path((h, p)): Path<(String, String)>) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.back(&p)?;
    Ok(Json(json!(e.play_view(&p)?)))
}

#[derive(Deserialize)]
struct Index {
    index: usize,
}

async fn rewind(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<Index>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.rewind_to(&p, b.index)?;
    Ok(Json(json!(e.play_view(&p)?)))
}

#[derive(Deserialize)]
struct JumpBody {
    beat: String,
    #[serde(default)]
    via: Option<yarner_engine::model::Path>,
}

async fn jump(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<JumpBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.jump(&p, &b.beat, b.via)?;
    Ok(Json(json!(e.play_view(&p)?)))
}

#[derive(Deserialize)]
struct RerunBody {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default = "here")]
    scope: Scope,
}

fn here() -> Scope {
    Scope::Here
}

async fn rerun(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<RerunBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.rerun(
        &p,
        b.prompt
            .filter(|t| !t.trim().is_empty())
            .map(|t| (t, b.scope)),
    )?;
    Ok(Json(json!(e.play_view(&p)?)))
}

#[derive(Deserialize)]
struct ForkBody {
    #[serde(default)]
    prompt: Option<String>,
}

async fn fork(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<ForkBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.fork(&p, b.prompt.filter(|t| !t.trim().is_empty()))?;
    Ok(Json(json!(e.play_view(&p)?)))
}

#[derive(Deserialize)]
struct VersionBody {
    beat: String,
}

async fn version(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<VersionBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.switch_version(&p, &b.beat)?;
    Ok(Json(json!(e.play_view(&p)?)))
}

async fn favor(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<ExitBody>,
) -> R<Json<Value>> {
    engine(&s, &h)?.favor(&p, b.exit);
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct SaveBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    slot: Option<u32>,
    /// The line of the beat on screen.
    #[serde(default)]
    line: Option<u32>,
}

async fn save(
    State(s): State<Shared>,
    Path((h, p)): Path<(String, String)>,
    Json(b): Json<SaveBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    Ok(Json(
        json!({ "id": e.save(&p, b.name.trim(), b.slot, b.line)? }),
    ))
}

async fn saves(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    Ok(Json(json!(engine(&s, &h)?.saves())))
}

#[derive(Deserialize)]
struct LoadBody {
    play: String,
}

async fn load(
    State(s): State<Shared>,
    Path((h, id)): Path<(String, String)>,
    Json(b): Json<LoadBody>,
) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    e.load(&id, &b.play)?;
    Ok(Json(json!(e.play_view(&b.play)?)))
}

async fn delete_save(
    State(s): State<Shared>,
    Path((h, id)): Path<(String, String)>,
) -> R<Json<Value>> {
    engine(&s, &h)?.delete_save(&id)?;
    Ok(Json(json!({})))
}

async fn map(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Query(q): Query<PlayQ>,
) -> R<Json<Value>> {
    Ok(Json(json!(engine(&s, &h)?.map(q.play.as_deref()))))
}

async fn get_prefs(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    Ok(Json(engine(&s, &h)?.prefs()))
}

async fn set_prefs(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Value>,
) -> R<Json<Value>> {
    engine(&s, &h)?.set_prefs(b)?;
    Ok(Json(json!({})))
}

async fn queue(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    let e = engine(&s, &h)?;
    Ok(Json(
        json!({ "jobs": e.queue().await, "idle": e.idle(), "writing": e.text_summary(20) }),
    ))
}

async fn export(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Json(b): Json<Passphrase>,
) -> R<Response> {
    if b.passphrase.chars().count() < 8 {
        return Err(bad("use a passphrase of at least 8 characters"));
    }
    let e = engine(&s, &h)?;
    let inst = s.install.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let (cur, _) = e.export(&inst, &b.passphrase, std::io::Cursor::new(Vec::new()))?;
        Ok::<_, EngineError>(cur.into_inner())
    })
    .await
    .map_err(|e| bad(e.to_string()))??;
    let mut res = Response::new(Body::from(bytes));
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"story.yarner\""),
    );
    Ok(res)
}

async fn export_report(State(s): State<Shared>, Path(h): Path<String>) -> R<Json<Value>> {
    Ok(Json(json!(engine(&s, &h)?.export_report())))
}

#[derive(Deserialize)]
struct Confirm {
    #[serde(default)]
    confirm: bool,
}

/// Delete every story here (asked "are you sure?" on the page; no passphrase: nothing is
/// opened or read, the encrypted files just go).
async fn clear_all(State(s): State<Shared>, Json(b): Json<Confirm>) -> R<Json<Value>> {
    if !b.confirm {
        return Err(bad("confirm to delete every story"));
    }
    let open: Vec<Open> = s
        .open
        .lock()
        .expect("open")
        .drain()
        .map(|(_, o)| o)
        .collect();
    for o in open {
        o.engine.shutdown().await;
    }
    let inst = s.install.clone();
    let n = tokio::task::spawn_blocking(move || inst.delete_all())
        .await
        .map_err(|e| bad(e.to_string()))??;
    Ok(Json(json!({ "deleted": n })))
}

/// The generator's models and what they can do (none without a generator), for the
/// setup screen's pickers.
fn gpu_json(s: &Shared) -> Json<Value> {
    Json(json!({ "state": *s.gpu.state.lock().expect("gpu state") }))
}

async fn gpu_get(State(s): State<Shared>) -> Json<Value> {
    gpu_json(&s)
}

#[derive(Deserialize)]
struct GpuBody {
    paused: bool,
}

/// Pause (give the GPU back) or resume. Pausing returns once every story's work is off
/// the GPU and the private thinfer-serve is stopped; resuming once it's back.
async fn gpu_set(State(s): State<Shared>, Json(b): Json<GpuBody>) -> R<Json<Value>> {
    let set = |v: &'static str| *s.gpu.state.lock().expect("gpu state") = v;
    let engines = || -> Vec<Engine> {
        let open = s.open.lock().expect("open stories");
        open.values().map(|o| o.engine.clone()).collect()
    };
    if b.paused {
        if s.gpu.held() {
            return Ok(gpu_json(&s));
        }
        set("pausing");
        tracing::info!("pause: giving the GPU back");
        // Hold first (engines check it before counting anything as failed), then wait
        // for their cancelled jobs to come back before the server goes.
        std::fs::write(&s.gpu.file, b"paused").map_err(|e| bad(e.to_string()))?;
        s.gpu.hold.send_modify(|h| *h = true);
        let all = engines();
        for e in &all {
            e.hold(true);
        }
        let t0 = Instant::now();
        while all.iter().any(|e| e.running() > 0) && t0.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if let Some(up) = &s.gpu.up {
            let mut up = up.clone();
            let _ = tokio::time::timeout(Duration::from_secs(30), up.wait_for(|u| !*u)).await;
        }
        set("paused");
    } else {
        if !s.gpu.held() && *s.gpu.state.lock().expect("gpu state") == "running" {
            return Ok(gpu_json(&s));
        }
        set("resuming");
        tracing::info!("resume: taking the GPU again");
        let _ = std::fs::remove_file(&s.gpu.file);
        // Tell the supervisor first: the engines start once the server answers.
        let wait = s.gpu.up.clone();
        s.gpu.hold.send_modify(|h| *h = false);
        if let Some(mut up) = wait {
            let _ = tokio::time::timeout(Duration::from_secs(150), up.wait_for(|u| *u)).await;
        }
        if s.gpu.held() {
            // Paused again meanwhile.
            return Ok(gpu_json(&s));
        }
        for e in engines() {
            e.hold(false);
        }
        set("running");
    }
    Ok(gpu_json(&s))
}

async fn models(State(s): State<Shared>) -> Json<Value> {
    let list = if s.generator {
        s.backend.models().await
    } else {
        vec![]
    };
    Json(json!({ "models": list }))
}

#[derive(Deserialize)]
struct Vault {
    #[serde(default)]
    password: Option<String>,
}

/// The generator's adapter vault (LoRAs) for every story: its password is held in
/// memory only, never written anywhere; `null` forgets it.
async fn vault_set(State(s): State<Shared>, Json(b): Json<Vault>) -> Json<Value> {
    s.backend.set_vault(b.password);
    // A password the vault refuses isn't kept (else every picture silently went without
    // its adapters while the page said the vault was on).
    if s.backend.vault_check().await == Some(false) {
        s.backend.set_vault(None);
        return Json(json!({ "set": false, "refused": true }));
    }
    Json(json!({ "set": s.backend.vault_set() }))
}

async fn vault_get(State(s): State<Shared>) -> Json<Value> {
    Json(json!({ "set": s.backend.vault_set() }))
}

#[derive(Deserialize)]
struct AdaptersQ {
    model: String,
}

/// The adapters the vault holds for a model, to pick from (names shown only here, to
/// whoever set the password). `adapters: null`: no vault open.
async fn adapters(
    State(s): State<Shared>,
    axum::extract::Query(q): axum::extract::Query<AdaptersQ>,
) -> Json<Value> {
    Json(json!({ "adapters": s.backend.adapters(q.model).await }))
}

async fn api_missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "no such API route".into())
}

#[derive(Deserialize)]
struct WsQ {
    #[serde(default)]
    play: Option<String>,
}

/// Live events for one client. While connected, the story counts as attended.
async fn ws(
    State(s): State<Shared>,
    Path(h): Path<String>,
    Query(q): Query<WsQ>,
    up: WebSocketUpgrade,
) -> R<Response> {
    let e = engine(&s, &h)?;
    if let Some(p) = &q.play {
        e.activate(p);
    }
    Ok(up.on_upgrade(move |sock| client(s, h, e, q.play, sock)))
}

async fn client(s: Shared, h: String, e: Engine, play: Option<String>, mut sock: WebSocket) {
    if let Some(o) = s.open.lock().expect("open").get_mut(&h) {
        o.clients += 1;
    }
    let mut events = e.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(ev) => {
                    if sock.send(WsMessage::Text(serde_json::to_string(&ev).expect("json").into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = sock.send(WsMessage::Text(r#"{"type":"resync"}"#.into())).await;
                }
                Err(_) => break,
            },
            _ = tick.tick() => {
                let q = e.queue_for(play.as_deref()).await;
                let msg = json!({"type": "queue", "jobs": q, "idle": e.idle(), "writing": e.text_summary(20)});
                if sock.send(WsMessage::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
                if let Some(o) = s.open.lock().expect("open").get_mut(&h) {
                    o.last_activity = Instant::now();
                }
            }
            m = sock.recv() => match m {
                Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    if let Some(o) = s.open.lock().expect("open").get_mut(&h) {
        o.clients = o.clients.saturating_sub(1);
        o.last_activity = Instant::now();
    }
}

/// Lock stories nobody is attending: idle past the timeout and not keeping on.
pub async fn reaper(s: Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tick.tick().await;
        let gone: Vec<Open> = {
            let now = Instant::now();
            let mut open = s.open.lock().expect("open");
            // Time's up on "keep generating": back to making only what's near the reader
            // (even with a page still open somewhere).
            for o in open.values_mut() {
                if o.keep_until.is_some_and(|k| now > k) {
                    o.keep_until = None;
                    o.engine.set_fill(false);
                    o.engine.set_depth(s.engine_cfg.speculate_depth);
                }
            }
            let doomed: Vec<String> = open
                .iter()
                .filter(|(_, o)| o.clients == 0)
                .filter(|(_, o)| now.duration_since(o.last_activity) > s.idle_timeout)
                .filter(|(_, o)| o.keep_until.is_none_or(|k| now > k))
                .map(|(h, _)| h.clone())
                .collect();
            doomed.iter().filter_map(|h| open.remove(h)).collect()
        };
        for o in gone {
            o.engine.shutdown().await;
            tracing::info!("locked an idle story");
        }
    }
}
