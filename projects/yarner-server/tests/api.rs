//! The HTTP API end to end, in process, against the mock generator.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use yarner_engine::Config;
use yarner_format::{Install, KdfParams};
use yarner_server::app::{self, AppState};
use yarner_testkit::mock::{MockThinfer, Timing};

fn app(dir: &std::path::Path) -> Router {
    let state = Arc::new(AppState {
        install: Arc::new(Install::open_with(dir, KdfParams::insecure_fast()).unwrap()),
        backend: MockThinfer::new(Timing::instant()),
        engine_cfg: Config::default(),
        idle_timeout: Duration::from_secs(60),
        open: Mutex::new(Default::default()),
        generator: true,
        gpu: app::Gpu::new(dir.join("gpu-paused")).0,
    });
    app::router(state).layer(axum::middleware::from_fn(app::headers))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (s, _, b) = raw(
        app,
        method,
        uri,
        body.map(|b| b.to_string().into_bytes()),
        &[],
    )
    .await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn raw(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Vec<u8>>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some()
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let (parts, body) = res.into_parts();
    (
        parts.status,
        parts.headers,
        to_bytes(body, usize::MAX).await.unwrap().to_vec(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn story_lifecycle_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());

    let (s, v) = call(
        &app,
        "POST",
        "/api/stories",
        Some(json!({"title": "Lamp", "passphrase": "short"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = call(
        &app,
        "POST",
        "/api/stories",
        Some(json!({"title": "Lamp", "passphrase": "correct horse"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let h = v["handle"].as_str().unwrap().to_string();
    let base = format!("/api/s/{h}");

    // Setup: prompt, an object with an uploaded reference picture.
    let (s, _) = call(
        &app,
        "POST",
        &format!("{base}/prompt"),
        Some(json!({"id": "story", "text": "A lighthouse mystery."})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let png = yarner_testkit::png::placeholder(16, 16, 42);
    let (s, _, b) = raw(
        &app,
        "POST",
        &format!("{base}/media?caption=front&objects=Mira"),
        Some(png.clone()),
        &[("content-type", "image/png")],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let mid = serde_json::from_slice::<Value>(&b).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, v) = call(&app, "POST", &format!("{base}/object"), Some(json!({"name": "Mira", "description": "The keeper.", "refs": [{"media": mid, "caption": "front"}]}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, setup) = call(&app, "GET", &format!("{base}/setup"), None).await;
    assert_eq!(setup["objects"][0]["name"], "Mira");
    assert_eq!(setup["prompts"][0]["text"], "A lighthouse mystery.");

    // Media round trip, whole and ranged, never cacheable.
    let (s, hd, b) = raw(&app, "GET", &format!("{base}/media/{mid}"), None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, png);
    assert_eq!(hd[header::CACHE_CONTROL], "no-store");
    let (s, hd, b) = raw(
        &app,
        "GET",
        &format!("{base}/media/{mid}"),
        None,
        &[("range", "bytes=2-9")],
    )
    .await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(b, png[2..10]);
    assert_eq!(
        hd[header::CONTENT_RANGE],
        format!("bytes 2-9/{}", png.len())
    );

    // A painted reference picture, as the setup screen makes them.
    let (s, v) = call(
        &app,
        "POST",
        &format!("{base}/media/generate"),
        Some(json!({"prompt": "portrait in oils", "caption": "painted", "objects": ["Mira"], "refs": [mid]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // Play: advance until moving, a few times.
    let (_, v) = call(
        &app,
        "POST",
        &format!("{base}/plays"),
        Some(json!({"name": ""})),
    )
    .await;
    let p = v["id"].as_str().unwrap().to_string();
    let mut moved = 0;
    for _ in 0..400 {
        let (s, v) = call(
            &app,
            "POST",
            &format!("{base}/plays/{p}/advance"),
            Some(json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        if v["result"]["type"] == "moved" {
            moved += 1;
            if moved == 1 {
                // The opening beat gets its picture.
                let mut shown = Value::Null;
                for _ in 0..300 {
                    let (_, view) = call(&app, "GET", &format!("{base}/plays/{p}"), None).await;
                    shown = view["beat"]["shows"].clone();
                    if shown["status"] == "ready" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(shown["status"], "ready", "opening picture: {shown}");
            }
            if moved == 5 {
                break;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(moved, 5);
    let (_, view) = call(&app, "GET", &format!("{base}/plays/{p}"), None).await;
    assert_eq!(view["depth"], 5);
    assert!(!view["beat"]["lines"].as_array().unwrap().is_empty());
    let (_, map) = call(&app, "GET", &format!("{base}/map?play={p}"), None).await;
    assert!(map["nodes"].as_array().unwrap().len() >= 5);

    // Save, export, lock, then unlock again by passphrase.
    let (s, _) = call(
        &app,
        "POST",
        &format!("{base}/plays/{p}/save"),
        Some(json!({"name": "one"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, hd, zip) = raw(
        &app,
        "POST",
        &format!("{base}/export"),
        Some(
            json!({"passphrase": "share it now"})
                .to_string()
                .into_bytes(),
        ),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(hd[header::CONTENT_TYPE], "application/zip");
    let (s, report) = call(&app, "GET", &format!("{base}/export/report"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(report["beats"].as_u64().unwrap() >= 5);
    let (s, v) = call(&app, "GET", &format!("{base}/nope"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert!(v["error"].is_string());
    let (s, _) = call(&app, "GET", "/api/whatever", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "POST", &format!("{base}/lock"), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, "GET", &base, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (_, v) = call(
        &app,
        "POST",
        "/api/unlock",
        Some(json!({"passphrase": "nope nope"})),
    )
    .await;
    assert!(v["stories"].as_array().unwrap().is_empty());
    let (_, v) = call(
        &app,
        "POST",
        "/api/unlock",
        Some(json!({"passphrase": "correct horse"})),
    )
    .await;
    let h2 = v["stories"][0]["handle"].as_str().unwrap();
    assert_ne!(h2, h);
    let (_, info) = call(&app, "GET", &format!("/api/s/{h2}"), None).await;
    assert_eq!(info["title"], "Lamp");
    assert_eq!(info["saves"].as_array().unwrap().len(), 1);

    // Import the export as a new story.
    let (s, _, b) = raw(
        &app,
        "POST",
        "/api/import",
        Some(zip),
        &[
            ("x-bundle-passphrase", "share it now"),
            ("x-passphrase", "another pass"),
            ("content-type", "application/zip"),
        ],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let (_, v) = call(
        &app,
        "POST",
        "/api/unlock",
        Some(json!({"passphrase": "another pass"})),
    )
    .await;
    assert_eq!(v["stories"][0]["title"], "Lamp");
}

#[test]
fn ranges() {
    use yarner_server::app::parse_range;
    assert_eq!(parse_range("bytes=0-9", 100), Some((0, 10)));
    assert_eq!(parse_range("bytes=90-", 100), Some((90, 100)));
    assert_eq!(parse_range("bytes=-10", 100), Some((90, 100)));
    assert_eq!(parse_range("bytes=0-999", 100), Some((0, 100)));
    assert_eq!(parse_range("bytes=100-", 100), None);
    assert_eq!(parse_range("bytes=-", 100), None);
}

/// Pause gives the GPU back (kept in the data folder across restarts); resume takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_the_gpu() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let (s, v) = call(&app, "GET", "/api/gpu", None).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("running")));
    let (_, v) = call(&app, "POST", "/api/gpu", Some(json!({ "paused": true }))).await;
    assert_eq!(v["state"], "paused");
    assert!(dir.path().join("gpu-paused").exists());
    // A restart comes up paused.
    let again = app::Gpu::new(dir.path().join("gpu-paused")).0;
    assert!(again.held());
    let (_, v) = call(&app, "POST", "/api/gpu", Some(json!({ "paused": false }))).await;
    assert_eq!(v["state"], "running");
    assert!(!dir.path().join("gpu-paused").exists());
}
