//! Hermetic axum handler tests for the dashboard router.
//!
//! Every test drives the real `routes::router` through `tower::ServiceExt::oneshot`
//! (no TCP listener, no network). A per-test `tempfile::TempDir` + `EnvGuard`
//! pins `HOME` / `RTRT_MEMORY_PATH` / `RTRT_CONFIG` to a scratch directory so the
//! real `~/.rtrt` is never touched. The `ENV_MUTEX` serializes env-mutating
//! tests so parallel `#[tokio::test]` threads can't race on those vars.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use rtrt_memory::MemoryStore;
use rtrt_providers::Gateway;
use tokio::sync::{Mutex, broadcast};
use tower::ServiceExt;

use crate::routes::router;
use crate::state::AppState;

/// Serializes env-mutating tests so parallel test threads never race on
/// `HOME` / `RTRT_*` overrides. Acquired by [`EnvGuard::new`] for the whole
/// test body; the guard restores the originals on drop before releasing.
static ENV_MUTEX: StdMutex<()> = StdMutex::new(());

/// RAII guard: while live, points `HOME` / `RTRT_MEMORY_PATH` at `tmp_home`
/// and clears `RTRT_CONFIG` (so config falls back to `<tmp_home>/.rtrt`).
/// Restores every var on drop.
struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn new(tmp_home: &std::path::Path) -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(tmp_home).ok();
        let mem = tmp_home.join("memory.sqlite");
        let overrides: [(&'static str, Option<std::ffi::OsString>); 3] = [
            ("HOME", Some(tmp_home.as_os_str().to_owned())),
            ("RTRT_MEMORY_PATH", Some(mem.into_os_string())),
            ("RTRT_CONFIG", None),
        ];
        let mut saved = Vec::with_capacity(overrides.len());
        for (key, new_val) in overrides {
            saved.push((key, std::env::var_os(key)));
            // SAFETY: `ENV_MUTEX` serializes every env-touching dashboard test,
            // so no other thread reads or writes these vars while the guard is
            // live; originals are restored in `Drop` before the lock releases.
            unsafe {
                match new_val {
                    Some(v) => std::env::set_var(key, &v),
                    None => std::env::remove_var(key),
                }
            }
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, orig) in self.saved.iter().rev() {
            // SAFETY: same single-test serialization as in `EnvGuard::new`.
            unsafe {
                match orig {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

/// Build a minimal `AppState` backed by a fresh SQLite store at
/// `<tmp_home>/memory.sqlite`. No embedder / no auto-capture / no daemons, so
/// the router is exercised in isolation.
fn test_state(tmp_home: &std::path::Path) -> AppState {
    let mem_path = tmp_home.join("memory.sqlite");
    let memory = MemoryStore::open(&mem_path).expect("open memory store");
    let (events, _) = broadcast::channel::<String>(256);
    AppState {
        gateway: Arc::new(Gateway::from_env()),
        prompts: None,
        memory: Some(Arc::new(Mutex::new(memory))),
        auto_capture: false,
        auto_redact: false,
        default_project: "default".to_string(),
        session_id: "test-session".to_string(),
        dedup_window_sec: 0,
        events,
        embedder: None,
        cluster_cache: Arc::new(Mutex::new(HashMap::new())),
        brainh_cache: Arc::new(Mutex::new(HashMap::new())),
        level_tokens: Arc::new(Mutex::new(HashMap::new())),
        memory_path: mem_path,
        embedding_jobs: Arc::new(StdMutex::new(HashSet::new())),
    }
}

/// Drive `app` with `req` via `oneshot`, unwrapping the `Infallible` result.
async fn call(app: axum::Router, req: Request<Body>) -> axum::response::Response {
    app.oneshot(req)
        .await
        .expect("axum router service is infallible")
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn json(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_text(resp).await).unwrap()
}

#[tokio::test]
async fn healthz_returns_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/healthz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_text(resp).await, "ok");
}

#[tokio::test]
async fn stats_returns_zeroed_json() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/api/stats")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["input_saved"], 0);
    assert_eq!(v["output_saved"], 0);
    assert!(v["provider"].is_null());
}

#[tokio::test]
async fn projects_lists_memory_buckets() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        let id = store
            .save("demo", "note", "remember to ship the feature")
            .unwrap();
        store.tag_row(id, Some("sess-1"), Some("sha1")).unwrap();
    }
    let app = router(state, None);
    let resp = call(app, get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let arr = v.as_array().expect("projects is an array");
    let demo = arr
        .iter()
        .find(|p| p["name"] == "demo")
        .expect("demo project present");
    assert_eq!(demo["mem_count"], 1);
}

#[tokio::test]
async fn compression_config_get_post_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    // Global write: POST lite, persisted to <tmp>/.rtrt/config.toml.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/compression/config",
            r#"{"level":"lite"}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["level"], "lite");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["scope"], "global");

    // GET reads the persisted global override back.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/compression/config")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["level"], "lite");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["scope"], "global");

    // Disable via the "off" pseudo-level and confirm it sticks.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/compression/config",
            r#"{"level":"off"}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json_body(resp).await["level"], "off");

    let app = router(state, None);
    let resp = call(app, get("/api/compression/config")).await;
    let v = json_body(resp).await;
    assert_eq!(v["level"], "off");
    assert_eq!(v["enabled"], false);
}

#[tokio::test]
async fn memory_sessions_groups_by_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        let id = store.save("demo", "note", "first row").unwrap();
        store.tag_row(id, Some("sess-1"), Some("sha1")).unwrap();
        let id = store.save("demo", "note", "second row").unwrap();
        store.tag_row(id, Some("sess-1"), Some("sha2")).unwrap();
        let id = store.save("demo", "note", "other session").unwrap();
        store.tag_row(id, Some("sess-2"), Some("sha3")).unwrap();
    }
    let app = router(state, None);
    let resp = call(app, get("/api/memory/sessions?project=demo")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["project"], "demo");
    let sessions = v["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 2);
    let s1 = sessions
        .iter()
        .find(|s| s["session_id"] == "sess-1")
        .expect("sess-1 present");
    assert_eq!(s1["count"], 2);
}

#[tokio::test]
async fn memory_sessions_empty_project_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/api/memory/sessions?project=ghost")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["project"], "ghost");
    assert_eq!(v["total"], 0);
    assert!(v["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bearer_guard_blocks_api_without_token() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let resp = call(app, get("/api/stats")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_text(resp).await;
    assert!(body.contains("unauthorized"), "{body}");
}

#[tokio::test]
async fn bearer_guard_advertises_challenge() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let resp = call(app, get("/api/stats")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let challenge = resp
        .headers()
        .get("WWW-Authenticate")
        .expect("WWW-Authenticate header present");
    assert!(
        challenge.to_str().unwrap().contains("Bearer"),
        "expected Bearer challenge",
    );
}

#[tokio::test]
async fn bearer_guard_exempt_spa_shell_and_healthz() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let token = Some("s3cr3t".to_string());

    // "/" (SPA shell) bootstraps without a bearer header.
    let app = router(test_state(tmp.path()), token.clone());
    let resp = call(app, get("/")).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // "/healthz" is also token-exempt so liveness probes stay open.
    let app = router(test_state(tmp.path()), token);
    let resp = call(app, get("/healthz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn bearer_guard_accepts_correct_token() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/stats")
        .header(header::AUTHORIZATION, "Bearer s3cr3t")
        .body(Body::empty())
        .unwrap();
    let resp = call(app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn bearer_guard_rejects_wrong_token() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/stats")
        .header(header::AUTHORIZATION, "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    let resp = call(app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn spa_fallback_serves_deep_path_as_html() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/memory/search")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type set")
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("text/html"), "expected text/html, got {ct}",);
    assert!(!body_text(resp).await.is_empty());
}

#[tokio::test]
async fn spa_fallback_404_for_bogus_api() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/api/bogus")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
