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
    let arr = v["projects"].as_array().expect("projects is an array");
    let demo = arr
        .iter()
        .find(|p| p["name"] == "demo")
        .expect("demo project present");
    assert_eq!(demo["mem_count"], 1);
    assert_eq!(v["hidden_capture_buckets"], 0);
    assert_eq!(v["hidden_capture_bucket_rows"], 0);
}

/// A bucket named like a machine-generated session-hash pair (the confirmed
/// orphan shape: source transcript deleted, reattribution can never resolve
/// it) must not clutter the selector — but its rows stay in the store.
#[tokio::test]
async fn projects_hides_orphan_capture_buckets_but_keeps_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let orphan = "30877432d1026706d7e805da846a32c3-bb81e3c29b62179273c8eb5bb682575ec87a171a";
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for _ in 0..3 {
            store
                .save(orphan, "teammate-message", "stray subagent output")
                .unwrap();
        }
        store.save("realproject", "note", "actual work").unwrap();
    }
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let arr = v["projects"].as_array().unwrap();
    assert!(
        arr.iter().all(|p| p["name"] != orphan),
        "orphan bucket must not be listed"
    );
    assert!(arr.iter().any(|p| p["name"] == "realproject"));
    assert_eq!(v["hidden_capture_buckets"], 1);
    assert_eq!(v["hidden_capture_bucket_rows"], 3);

    // Also exposed via the dedicated hidden-buckets endpoint for the UI's
    // reassign picker.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/projects/hidden")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let hidden = json_body(resp).await;
    let hidden = hidden.as_array().unwrap();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0]["name"], orphan);
    assert_eq!(hidden[0]["mem_count"], 3);

    // Nothing was deleted — the rows are still in the store under the
    // orphan's own name, untouched.
    let store = state.memory.as_ref().unwrap().lock().await;
    let projects = store.projects().unwrap();
    let (_, count, _) = projects
        .iter()
        .find(|(n, _, _)| n == orphan)
        .expect("orphan bucket still present in the store");
    assert_eq!(*count, 3);
}

/// A registered project always shows, even if its name happens to match the
/// capture-bucket shape (e.g. someone genuinely named a project `agent-42`).
#[tokio::test]
async fn projects_registered_entry_is_never_hidden_even_if_name_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::PUT,
            "/api/projects",
            r#"{"name":"agent-42","path":null}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    {
        let store = state.memory.as_ref().unwrap().lock().await;
        store.save("agent-42", "note", "real project work").unwrap();
    }
    let app = router(state, None);
    let resp = call(app, get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let arr = v["projects"].as_array().unwrap();
    assert!(
        arr.iter().any(|p| p["name"] == "agent-42"),
        "registered project must stay visible despite matching the capture-bucket shape"
    );
    assert_eq!(v["hidden_capture_buckets"], 0);
}

/// `POST /api/projects/reassign` folds an orphan bucket's rows into a real
/// project via a parameterized bulk UPDATE — the manual fallback for when
/// automatic reattribution can never resolve a parent.
#[tokio::test]
async fn projects_reassign_folds_orphan_rows_into_target() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let orphan = "agent-1234";
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for _ in 0..2 {
            store
                .save(orphan, "teammate-message", "stray subagent output")
                .unwrap();
        }
    }
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/projects/reassign",
            &format!(r#"{{"from":"{orphan}","to":"realproject"}}"#),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["moved"], 2);
    assert_eq!(v["from"], orphan);
    assert_eq!(v["to"], "realproject");

    let store = state.memory.as_ref().unwrap().lock().await;
    let projects = store.projects().unwrap();
    assert!(
        projects.iter().all(|(n, _, _)| n != orphan),
        "orphan bucket should be empty/gone after reassign"
    );
    let (_, count, _) = projects
        .iter()
        .find(|(n, _, _)| n == "realproject")
        .expect("target project now has the rows");
    assert_eq!(*count, 2);
}

/// `from` and `to` must both be present and differ — a same-name reassign is
/// a no-op the caller almost certainly didn't intend.
#[tokio::test]
async fn projects_reassign_rejects_same_from_and_to() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let app = router(state, None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/projects/reassign",
            r#"{"from":"demo","to":"demo"}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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

/// `mode=overview` builds the whole-project LOD index and mints one drill
/// token per bubble — the live path the Memory map actually uses.
#[tokio::test]
async fn memory_graph_overview_returns_bubbles_with_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for i in 0..6 {
            store
                .save(
                    "demo",
                    "note",
                    &format!("memory row number {i} about the deploy pipeline"),
                )
                .unwrap();
        }
    }
    let app = router(state, None);
    let resp = call(app, get("/api/memory/graph?project=demo&mode=overview")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["mode"], "overview");
    let clusters = v["clusters"].as_array().expect("clusters array");
    assert!(!clusters.is_empty(), "at least one bubble");
    for c in clusters {
        assert!(
            c["token"].as_str().is_some_and(|t| !t.is_empty()),
            "every bubble carries a drill token: {c:?}"
        );
    }
}

/// Drilling an overview bubble's token must resolve real memory nodes — this
/// is the ONLY drill-down path the shipped frontend uses.
#[tokio::test]
async fn memory_graph_token_drill_returns_members() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for i in 0..6 {
            store
                .save(
                    "demo",
                    "note",
                    &format!("memory row number {i} about the deploy pipeline"),
                )
                .unwrap();
        }
    }
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/memory/graph?project=demo&mode=overview")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let overview = json_body(resp).await;
    let token = overview["clusters"][0]["token"]
        .as_str()
        .expect("first bubble has a token")
        .to_string();

    let app = router(state, None);
    let resp = call(app, get(&format!("/api/memory/graph?token={token}"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    // A 6-row bubble is well under the dynamic leaf cutoff, so it renders
    // straight to individual memory nodes rather than sub-bubbling further.
    assert_eq!(
        v["mode"], "leaf",
        "small bucket drills straight to a leaf: {v:?}"
    );
    assert!(
        !v["nodes"].as_array().unwrap().is_empty(),
        "leaf carries real memory nodes"
    );
}

/// The retired `?cluster=<id>` drill-down must never silently resolve — it
/// used to look the root id up in a differently-keyed (and since-removed)
/// index than the one the overview minted, which mostly returned an empty
/// `{nodes:[],edges:[]}` for a valid-looking root id. It must now return an
/// explicit `410 Gone` so a straggling client learns to re-fetch and drill
/// via `token` instead of rendering a bogus empty cluster.
#[tokio::test]
async fn memory_graph_legacy_cluster_query_returns_410() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/api/memory/graph?project=demo&cluster=123")).await;
    assert_eq!(resp.status(), StatusCode::GONE);
    let body = body_text(resp).await;
    assert!(
        body.contains("token"),
        "hints the client to re-fetch via token, got: {body}"
    );
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
