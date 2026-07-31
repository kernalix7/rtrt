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

/// The sandboxed config file for a test's temp home. [`EnvGuard`] pins
/// `RTRT_CONFIG` to exactly this path, so it is where the config endpoints
/// read and write on every platform.
fn config_file(tmp_home: &std::path::Path) -> std::path::PathBuf {
    tmp_home.join(".rtrt").join("config.toml")
}

/// RAII guard: while live, points `HOME` / `RTRT_MEMORY_PATH` / `RTRT_CONFIG`
/// at `tmp_home`. Restores every var on drop.
///
/// `RTRT_CONFIG` is pinned EXPLICITLY rather than cleared: `Config::default_path`
/// falls back to `dirs::home_dir()`, which reads `HOME` on Unix but `USERPROFILE`
/// (or the known-folder API) on Windows — so clearing it left the config path
/// resolving to the REAL profile there, and a test that writes config would
/// escape the sandbox. Pinning the var makes the sandbox hold on every platform,
/// matching how `crates/rtrt-cli/tests/cli.rs` already isolates the CLI.
struct EnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn new(tmp_home: &std::path::Path) -> Self {
        let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::create_dir_all(tmp_home).ok();
        let mem = tmp_home.join("memory.sqlite");
        let cfg = config_file(tmp_home);
        let overrides: [(&'static str, Option<std::ffi::OsString>); 3] = [
            ("HOME", Some(tmp_home.as_os_str().to_owned())),
            ("RTRT_MEMORY_PATH", Some(mem.into_os_string())),
            ("RTRT_CONFIG", Some(cfg.into_os_string())),
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

/// Reproduces the real-world "one mega-bubble eats the map" problem at small
/// scale: 300 memories share NO lexical token with one another (each body is
/// built from row-index-unique words), so the lexical clusterer cannot merge
/// them and folds the overflow into a single catch-all — well past
/// [`crate::state::STALL_DOMINANCE`] of the project (in this fixture, over
/// 80%), exactly like the 70% "(기타)" bubble the live `00G_CADKernel` store
/// produced. The rows are round-robin tagged across 5 sessions so the
/// overview's anti-stall balancing (`balance_overview_dominance` in
/// `handlers/memgraph.rs`) has a metadata facet to redistribute the mass
/// along, mirroring the per-bubble drill's own facet fallback.
///
/// Asserts the invariant the fix exists for: no top-level bubble holds
/// `>= STALL_DOMINANCE` of the whole map, every bubble's sizes still sum to
/// `total_nodes` (no member dropped or duplicated), every bubble carries a
/// drill token, the split sub-bubbles are labeled by their session (a real
/// group, not another generic misc label), and drilling one of those tokens
/// still resolves to its members.
#[tokio::test]
async fn memory_graph_overview_balances_dominant_catchall_bubble() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    const TOTAL: usize = 300;
    const SESSIONS: usize = 5;
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for i in 0..TOTAL {
            let body = format!("uniqueword{i}xyz alphatok{i}abc betatok{i}def gammatok{i}ghi");
            let id = store.save("bigproj", "note", &body).unwrap();
            let session = format!("sess-{}", i % SESSIONS);
            store.tag_row(id, Some(&session), None).unwrap();
        }
    }
    let app = router(state.clone(), None);
    let resp = call(
        app,
        get("/api/memory/graph?project=bigproj&mode=overview&group=context&basis=lexical"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["mode"], "overview");
    let total_nodes = v["total_nodes"].as_u64().expect("total_nodes present");
    assert_eq!(total_nodes, TOTAL as u64, "no row dropped from the project");

    let clusters = v["clusters"].as_array().expect("clusters array");
    assert!(!clusters.is_empty());

    let cap = total_nodes as f64 * crate::state::STALL_DOMINANCE;
    let mut size_sum: u64 = 0;
    let mut saw_session_label = false;
    for c in clusters {
        let size = c["size"].as_u64().expect("size present");
        size_sum += size;
        let label = c["label"].as_str().unwrap_or_default();
        let pct = crate::state::STALL_DOMINANCE * 100.0;
        assert!(
            (size as f64) < cap,
            "bubble {label:?} holds {size}/{total_nodes} rows, >= the {pct:.0}% dominance cap \
             (the mega-bubble regression this test guards against)"
        );
        let token = c["token"].as_str().expect("every bubble carries a token");
        assert!(!token.is_empty());
        if let Some(label) = c["label"].as_str()
            && label.starts_with("sess-")
        {
            saw_session_label = true;
        }
        assert_ne!(
            c["label"], "(기타)",
            "split sub-bubbles must read as real groups, not a generic misc label"
        );
    }
    assert_eq!(
        size_sum, total_nodes,
        "size_sum across bubbles == total_nodes"
    );
    assert!(
        saw_session_label,
        "the balanced dominant bubble's children should be labeled by session (the facet used to split it)"
    );

    // Drilling one of the split, session-labeled sub-bubbles must still
    // resolve to real members (the token path is untouched by the fix). The
    // token was minted into `state.level_tokens`, so the drill request must
    // reuse the SAME state (a fresh `test_state` would have no record of it).
    let split_token = clusters
        .iter()
        .find(|c| c["label"].as_str().is_some_and(|l| l.starts_with("sess-")))
        .and_then(|c| c["token"].as_str())
        .expect("at least one session-labeled bubble with a token")
        .to_string();
    let app = router(state, None);
    let resp = call(app, get(&format!("/api/memory/graph?token={split_token}"))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let drilled = json_body(resp).await;
    let member_count: u64 = match drilled["mode"].as_str() {
        Some("leaf") => drilled["nodes"]
            .as_array()
            .map(|a| a.len() as u64)
            .unwrap_or(0),
        Some("group") => drilled["clusters"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["size"].as_u64()).sum())
            .unwrap_or(0),
        other => panic!("unexpected drill mode {other:?}"),
    };
    assert!(
        member_count > 0,
        "drilling the split bubble's token still returns members"
    );
}

/// The memory timeline's `role` filter is the coarse INPUT (the user's own
/// typed prompts) / OUTPUT (everything agent-produced) split described in
/// `rtrt_memory::role`. `role=input` must return only `user-prompt-submit` /
/// `user-prompt-expansion` rows, `role=output` must exclude them, an absent
/// `role` must return every row, and `total` must always match the returned
/// item count (no pagination drift between the count and paged queries).
#[tokio::test]
async fn timeline_role_filter_splits_input_and_output() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let input_kinds = [
        "user-prompt-submit",
        "user-prompt-submit",
        "user-prompt-expansion",
    ];
    let output_kinds = [
        "assistant-turn",
        "teammate-message",
        "stop",
        "subagent-stop",
    ];
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for kind in input_kinds {
            store.save("demo", kind, "typed by the user").unwrap();
        }
        for kind in output_kinds {
            store.save("demo", kind, "produced by an agent").unwrap();
        }
    }

    // role=input — only the user's own prompts.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/memory/timeline?project=demo&role=input")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["total"], input_kinds.len() as i64);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), input_kinds.len());
    assert!(
        items
            .iter()
            .all(|i| i["kind"] == "user-prompt-submit" || i["kind"] == "user-prompt-expansion")
    );

    // role=output — everything else, input rows excluded.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/memory/timeline?project=demo&role=output")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["total"], output_kinds.len() as i64);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), output_kinds.len());
    assert!(
        items
            .iter()
            .all(|i| i["kind"] != "user-prompt-submit" && i["kind"] != "user-prompt-expansion")
    );

    // Absent role — every row, and input+output counts add up to it.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/memory/timeline?project=demo")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let total_all = input_kinds.len() + output_kinds.len();
    assert_eq!(v["total"], total_all as i64);
    assert_eq!(v["items"].as_array().unwrap().len(), total_all);
    assert!(v["role"].is_null());

    // role composes with the existing sort=importance path too.
    let app = router(state, None);
    let resp = call(
        app,
        get("/api/memory/timeline?project=demo&role=input&sort=importance"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["total"], input_kinds.len() as i64);
    assert_eq!(v["items"].as_array().unwrap().len(), input_kinds.len());
}

/// The search/recall endpoint (`/api/memory/recall`) accepts the same `role`
/// filter as the timeline, so a query can be scoped to just the user's own
/// prompts or just agent output.
#[tokio::test]
async fn recall_role_filter_restricts_hits() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        store
            .save("demo", "user-prompt-submit", "fix the parser bug")
            .unwrap();
        store
            .save("demo", "assistant-turn", "fixed the parser bug")
            .unwrap();
        store
            .save("demo", "teammate-message", "parser bug report")
            .unwrap();
    }
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/memory/recall",
            r#"{"project":"demo","query":"parser bug","role":"input"}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h["kind"] == "user-prompt-submit"));

    let app = router(state, None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/memory/recall",
            r#"{"project":"demo","query":"parser bug","role":"output"}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h["kind"] != "user-prompt-submit"));
}

// ---------------------------------------------------------------------------
// Orchestration config — `[team]` roster + `[failover]` markers.
//
// The roster the binary ships is only a DEFAULT, so these tests assert the
// SHAPE of the exchange (shipped default is served, an invalid roster never
// reaches disk, a custom roster round-trips) without pinning any lane, tier,
// target or model name — those are config, and a config change must not be a
// test change.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn team_config_get_returns_the_shipped_default_roster() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);

    let resp = call(app, get("/api/team/config")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;

    // With no config file at all, the endpoint serves the shipped roster —
    // compared against the core's own default rather than a literal list.
    let shipped = rtrt_core::TeamConfig::default();
    let names: Vec<&str> = v["members"]
        .as_array()
        .expect("members is a list")
        .iter()
        .map(|m| m["name"].as_str().expect("lane name"))
        .collect();
    let expected: Vec<&str> = shipped.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, expected);
    assert_eq!(v["enabled"], shipped.enabled);
    assert_eq!(v["manager_provider"], shipped.manager_provider);

    // `tiers` is the CONFIGURED ladder (empty by default); `effective.tiers`
    // is the one actually in force, so the UI can tell "unset" from "none".
    assert!(v["tiers"].as_array().expect("tiers is a list").is_empty());
    let effective = v["effective"]["tiers"].as_array().expect("effective tiers");
    assert!(!effective.is_empty());
    assert!(effective.iter().all(|t| t["tier"].is_string()));
    // Every lane gets a resolved fallback walk.
    assert_eq!(
        v["effective"]["chains"]
            .as_object()
            .expect("chains is a map")
            .len(),
        shipped.members.len()
    );
    assert_eq!(v["scope"], "global");
    assert_eq!(v["custom"], false);
    assert_eq!(v["inherited"], false);

    // A GET must never create the config file.
    assert!(!config_file(tmp.path()).exists());
}

#[tokio::test]
async fn team_config_post_with_a_fallback_cycle_is_rejected_and_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);

    // Two lanes that fall back to each other: the walk would never terminate.
    let body = r#"{
        "enabled": true,
        "leader_order": ["first"],
        "members": [
            {"name":"first","target":"alpha","model":"alpha/one","mode":"cli",
             "roles":["lead"],"logical":null,"sibling":null,"tier":null,
             "fallback":["second"],"allow_impl":true,"flags":{}},
            {"name":"second","target":"beta","model":"beta/one","mode":"cli",
             "roles":["support"],"logical":null,"sibling":null,"tier":null,
             "fallback":["first"],"allow_impl":true,"flags":{}}
        ],
        "tiers": [],
        "policy": null
    }"#;
    let resp = call(app, json(Method::POST, "/api/team/config", body)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    let message = v["error"].as_str().expect("error message");
    assert!(
        message.contains("cycle"),
        "validator message should name the cycle, got: {message}"
    );

    // Nothing was persisted: the invalid roster never reached the file.
    assert!(
        !config_file(tmp.path()).exists(),
        "a rejected roster must not create or touch the config file"
    );
}

#[tokio::test]
async fn team_config_post_roundtrips_a_valid_roster() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    // A fully custom roster: names, tier names and the ladder shape are all
    // the caller's, none of them the shipped ones.
    let body = r#"{
        "enabled": true,
        "manager_provider": "local-manager",
        "manager_model": "tiny",
        "manager_base_url": "",
        "leader_order": ["primary", "backup"],
        "members": [
            {"name":"primary","target":"alpha","model":"alpha/one","mode":"cli",
             "roles":["lead","review"],"logical":"one","sibling":"backup","tier":null,
             "fallback":["backup"],"allow_impl":true,"flags":{"permission-mode":"plan"}},
            {"name":"backup","target":"beta","model":"beta/one","mode":"api",
             "roles":["support"],"logical":"one","sibling":"primary","tier":null,
             "fallback":[],"allow_impl":false,"flags":{}}
        ],
        "tiers": [
            {"tier":"quick","members":["primary"]},
            {"tier":"deep","members":["primary","backup"]}
        ],
        "policy": {
            "max_retries": 4,
            "redo_on_fallback": false,
            "prefer_sibling_on_quota": true,
            "record_provenance": false,
            "max_fallback_depth": 2,
            "default_tier": "quick",
            "design_only_tiers": ["deep"]
        }
    }"#;
    let app = router(state.clone(), None);
    let resp = call(app, json(Method::POST, "/api/team/config", body)).await;
    assert_eq!(resp.status(), StatusCode::OK, "valid roster should persist");

    // GET reads the persisted roster back, ladder order intact.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["enabled"], true);
    assert_eq!(v["manager_provider"], "local-manager");
    // An empty manager_base_url is stored as "unset", not as an empty string.
    assert!(v["manager_base_url"].is_null());
    assert_eq!(v["members"][0]["name"], "primary");
    assert_eq!(v["members"][0]["sibling"], "backup");
    assert_eq!(v["members"][0]["flags"]["permission-mode"], "plan");
    assert_eq!(v["members"][1]["allow_impl"], false);
    let tiers = v["tiers"].as_array().expect("tiers is a list");
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[0]["tier"], "quick");
    assert_eq!(tiers[1]["tier"], "deep");
    assert_eq!(v["policy"]["max_retries"], 4);
    assert_eq!(v["policy"]["default_tier"], "quick");
    // The design-only rung is reported as such, and the lane that may not
    // implement is allowed there.
    let effective = v["effective"]["tiers"].as_array().expect("effective tiers");
    let deep = effective
        .iter()
        .find(|t| t["tier"] == "deep")
        .expect("deep rung");
    assert_eq!(deep["design_only"], true);
    assert_eq!(v["effective"]["chains"]["primary"][0], "backup");

    // The roster really is on disk under `[team]`. Read it back from the path
    // the ENDPOINT reports rather than a reconstructed one, so the assertion
    // cannot quietly pass against a file the handler never wrote.
    let reported = v["path"].as_str().expect("config path reported");
    assert_eq!(
        std::path::Path::new(reported),
        config_file(tmp.path()),
        "the endpoint must write inside the test sandbox"
    );
    let raw = std::fs::read_to_string(reported).expect("config written");
    assert!(
        raw.contains("[team]"),
        "config should carry a [team] section"
    );
}

#[tokio::test]
async fn team_config_post_rejects_a_design_only_lane_in_an_implementing_tier() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);

    // `backup` may not implement, yet sits in a tier that is not design-only.
    let body = r#"{
        "enabled": true,
        "leader_order": ["primary"],
        "members": [
            {"name":"primary","target":"alpha","model":"alpha/one","mode":"cli",
             "roles":["lead"],"logical":null,"sibling":null,"tier":null,
             "fallback":[],"allow_impl":true,"flags":{}},
            {"name":"backup","target":"beta","model":"beta/one","mode":"cli",
             "roles":["support"],"logical":null,"sibling":null,"tier":null,
             "fallback":[],"allow_impl":false,"flags":{}}
        ],
        "tiers": [{"tier":"quick","members":["primary","backup"]}],
        "policy": null
    }"#;
    let resp = call(app, json(Method::POST, "/api/team/config", body)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    let message = v["error"].as_str().expect("error message");
    assert!(
        message.contains("design-only"),
        "validator message should explain the design-only conflict, got: {message}"
    );
    assert!(!config_file(tmp.path()).exists());
}

#[tokio::test]
async fn team_config_post_rejects_a_cross_logical_sibling() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);

    // A sibling pair is one model on two pools; these are two different models.
    let body = r#"{
        "enabled": true,
        "leader_order": ["primary"],
        "members": [
            {"name":"primary","target":"alpha","model":"alpha/one","mode":"cli",
             "roles":["lead"],"logical":"one","sibling":"backup","tier":null,
             "fallback":[],"allow_impl":true,"flags":{}},
            {"name":"backup","target":"beta","model":"beta/two","mode":"cli",
             "roles":["support"],"logical":"two","sibling":null,"tier":null,
             "fallback":[],"allow_impl":true,"flags":{}}
        ],
        "tiers": [],
        "policy": null
    }"#;
    let resp = call(app, json(Method::POST, "/api/team/config", body)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    let message = v["error"].as_str().expect("error message");
    assert!(
        message.contains("sibling"),
        "validator message should name the sibling, got: {message}"
    );
    assert!(!config_file(tmp.path()).exists());
}

#[tokio::test]
async fn team_config_scope_toggle_matches_the_other_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    // Register a real on-disk project so `resolve_project_repo` resolves it,
    // exactly as the other per-project settings require.
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::PUT,
            "/api/projects",
            &format!(
                r#"{{"name":"demo","path":"{}"}}"#,
                repo.to_string_lossy().replace('\\', "\\\\")
            ),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A project that pins nothing INHERITS the global roster, and says so with
    // the same scope triple as the other seven per-project settings.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config?project=demo")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["custom"], false);
    assert_eq!(v["inherited"], true);
    // Reading a project's roster must not conjure an override file for it.
    assert!(!repo.join(".rtrt").join("config.toml").exists());

    // The global scope reports nothing to inherit, same as every other endpoint.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config?project=global")).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], false);

    // "Follow global" on a project that has no override is a safe no-op that
    // re-reads the effective roster, exactly like the other clear paths.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        post_empty("/api/team/config?project=demo&scope=global"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], true);
    assert!(!v["members"].as_array().expect("members").is_empty());
    assert!(!repo.join(".rtrt").join("config.toml").exists());
}

/// A POST with no body — the "Follow global" clear path every scoped endpoint
/// exposes.
fn post_empty(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// A roster with no lane in common with the shipped one, so "the project's own"
/// and "the global" can never be confused for each other.
const PROJECT_ROSTER: &str = r#"{
    "enabled": true,
    "manager_provider": "project-manager",
    "manager_model": "project-model",
    "manager_base_url": "",
    "leader_order": ["project-lane"],
    "members": [
        {"name":"project-lane","target":"project-target","model":null,"mode":"cli",
         "roles":["lead"],"logical":null,"sibling":null,"tier":null,
         "fallback":[],"allow_impl":true,"flags":{}}
    ],
    "tiers": [],
    "policy": null
}"#;

/// Register `demo` as an on-disk project rooted at `<tmp>/repo` and return it.
async fn register_demo_project(
    state: crate::state::AppState,
    tmp: &std::path::Path,
) -> std::path::PathBuf {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let resp = call(
        router(state, None),
        json(
            Method::PUT,
            "/api/projects",
            &format!(
                r#"{{"name":"demo","path":"{}"}}"#,
                repo.to_string_lossy().replace('\\', "\\\\")
            ),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    repo
}

#[tokio::test]
async fn team_config_writes_and_clears_a_project_override() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let repo = register_demo_project(state.clone(), tmp.path()).await;
    let project_file = repo.join(".rtrt").join("config.toml");

    // A coexisting override written first, so the clear below has a neighbour
    // it must not disturb.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/compression/config?project=demo",
            r#"{"level":"ultra"}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Writing with a project selected pins THIS project's roster.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/team/config?project=demo",
            PROJECT_ROSTER,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");
    assert_eq!(v["custom"], true);
    assert_eq!(v["inherited"], false);
    // The reported path is the file the write actually landed in.
    assert_eq!(v["path"], project_file.to_string_lossy().into_owned());

    // GET through the project scope now reads the override back…
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config?project=demo")).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");
    assert_eq!(v["manager_provider"], "project-manager");
    assert_eq!(v["members"][0]["name"], "project-lane");

    // …while the GLOBAL scope still serves the shipped roster untouched.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config")).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    let shipped = rtrt_core::TeamConfig::default();
    assert_eq!(v["manager_provider"], shipped.manager_provider);
    assert_eq!(v["members"][0]["name"], shipped.members[0].name);

    // `?scope=global` clears ONLY the team override; the compression override
    // in the same file survives.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        post_empty("/api/team/config?project=demo&scope=global"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], true);
    assert_eq!(v["manager_provider"], shipped.manager_provider);

    let stored = rtrt_core::Config::load_project(&repo).unwrap();
    assert!(stored.team.is_none(), "the team override was cleared");
    assert!(
        stored.compression.is_some(),
        "clearing the roster must not touch a coexisting override"
    );
}

#[tokio::test]
async fn a_project_roster_that_would_be_invalid_is_rejected_and_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let repo = register_demo_project(state.clone(), tmp.path()).await;

    // A ladder rung naming a lane this roster does not define — the effective
    // roster would be invalid, so the write must not happen at all.
    let body = r#"{
        "enabled": true,
        "leader_order": ["kept"],
        "members": [
            {"name":"kept","target":"one","model":null,"mode":"cli",
             "roles":["lead"],"logical":null,"sibling":null,"tier":null,
             "fallback":[],"allow_impl":true,"flags":{}}
        ],
        "tiers": [{"tier":"rung","members":["dropped"]}],
        "policy": null
    }"#;
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(Method::POST, "/api/team/config?project=demo", body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    let message = v["error"].as_str().expect("error message");
    // The validator's own message, not a paraphrase.
    assert!(
        message.contains("dropped") && message.contains("unknown member"),
        "expected the validator's message, got: {message}"
    );

    // Nothing reached disk: no project override file, and the project still
    // inherits the global roster.
    assert!(!repo.join(".rtrt").join("config.toml").exists());
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config?project=demo")).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
}

#[tokio::test]
async fn failover_config_scope_matches_the_team_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let repo = register_demo_project(state.clone(), tmp.path()).await;

    // A global policy the project will shadow.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/failover/config",
            r#"{"fatal":["global marker"],"quota":[],"transient":[],"transient_retries":3}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Before any project write, the project inherits it.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/failover/config?project=demo")).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], true);
    assert_eq!(v["fatal"][0], "global marker");

    // The project pins its own policy: whole-section replacement, so the
    // global marker and retry count are gone rather than merged in.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/failover/config?project=demo",
            r#"{"fatal":["project marker"],"quota":[],"transient":[],"transient_retries":null}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");

    let app = router(state.clone(), None);
    let resp = call(app, get("/api/failover/config?project=demo")).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");
    assert_eq!(v["fatal"].as_array().expect("fatal").len(), 1);
    assert_eq!(v["fatal"][0], "project marker");
    assert!(v["transient_retries"].is_null());

    // The global policy is untouched by the project write.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/failover/config")).await;
    let v = json_body(resp).await;
    assert_eq!(v["fatal"][0], "global marker");
    assert_eq!(v["transient_retries"], 3);

    // Clearing returns the project to the global policy and removes the now
    // empty override file, keeping the repo clean.
    let app = router(state.clone(), None);
    let resp = call(
        app,
        post_empty("/api/failover/config?project=demo&scope=global"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["fatal"][0], "global marker");
    assert!(!repo.join(".rtrt").join("config.toml").exists());
}

#[tokio::test]
async fn failover_config_get_post_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    // Defaults: no marker overrides at all.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/failover/config")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert!(v["fatal"].as_array().expect("fatal").is_empty());
    assert!(v["transient_retries"].is_null());
    assert_eq!(v["scope"], "global");

    let app = router(state.clone(), None);
    let resp = call(
        app,
        json(
            Method::POST,
            "/api/failover/config",
            r#"{"fatal":["contract expired"," "],"quota":["seat limit reached"],
                "transient":[],"transient_retries":1,"backoff_divisor":60,"backoff_ms":null}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let app = router(state.clone(), None);
    let resp = call(app, get("/api/failover/config")).await;
    let v = json_body(resp).await;
    // The blank marker a form leaves behind is dropped, not persisted.
    assert_eq!(
        v["fatal"].as_array().expect("fatal").len(),
        1,
        "blank markers should be dropped"
    );
    assert_eq!(v["fatal"][0], "contract expired");
    assert_eq!(v["quota"][0], "seat limit reached");
    assert_eq!(v["transient_retries"], 1);
    assert_eq!(v["backoff_divisor"], 60);
    assert!(v["backoff_ms"].is_null());
}

#[tokio::test]
async fn failover_config_rejects_a_zero_backoff_divisor() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);

    let resp = call(
        app,
        json(
            Method::POST,
            "/api/failover/config",
            r#"{"fatal":[],"quota":[],"transient":[],"backoff_divisor":0}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    assert!(
        v["error"]
            .as_str()
            .expect("error message")
            .contains("backoff_divisor")
    );
    assert!(!config_file(tmp.path()).exists());
}

#[tokio::test]
async fn orchestration_page_assets_are_served_and_deep_linkable() {
    let tmp = tempfile::tempdir().unwrap();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    // The page's script is embedded in the binary and served like the other
    // app assets: `no-cache`, so a rebuilt UI shows up on the next load.
    let app = router(state.clone(), None);
    let resp = call(app, get("/assets/js/orchestration.js")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    let script = body_text(resp).await;
    assert!(script.contains("loadOrchestration"));

    // The shell loads it, and the page + nav entry exist in the markup.
    let app = router(state.clone(), None);
    let html = body_text(call(app, get("/")).await).await;
    assert!(html.contains("/assets/js/orchestration.js"));
    assert!(html.contains("id=\"page-orchestration\""));
    assert!(html.contains("data-page=\"orchestration\""));

    // Deep link: /orchestration falls through to the SPA shell so a refresh
    // (or a shared URL) restores the page instead of 404ing.
    let app = router(state, None);
    let resp = call(app, get("/orchestration")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/html")
    );
}
