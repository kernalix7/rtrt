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

use crate::state::AppState;

const TEST_TOKEN: &str = "dashboard-test-token";

fn router(state: AppState, token: Option<String>) -> axum::Router {
    crate::routes::router(state, Some(token.unwrap_or_else(|| TEST_TOKEN.to_string())))
}

/// Serializes env-mutating tests so parallel test threads never race on
/// `HOME` / `RTRT_*` overrides. Acquired by [`EnvGuard::new`] for the whole
/// test body; the guard restores the originals on drop before releasing.
static ENV_MUTEX: StdMutex<()> = StdMutex::new(());
static TEST_SLUG: StdMutex<String> = StdMutex::new(String::new());

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
        let overrides: [(&'static str, Option<std::ffi::OsString>); 8] = [
            ("HOME", Some(tmp_home.as_os_str().to_owned())),
            ("RTRT_MEMORY_PATH", Some(mem.into_os_string())),
            ("RTRT_CONFIG", Some(cfg.into_os_string())),
            ("RTRT_OPENAI_COMPAT_URL", None),
            ("RTRT_PROVIDER_BASE_URL", None),
            ("RTRT_OPENAI_COMPAT_PROVIDER", None),
            ("OPENAI_API_KEY", None),
            ("RTRT_DASHBOARD_TOKEN", None),
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
/// A temp dir whose path is canonical.
///
/// macOS reaches the system temp dir through `/var -> /private/var`, and project
/// identity derives from the canonical path, so a raw handle path makes every
/// derived slug disagree with the home the catalog was built from.
struct CanonicalTempDir {
    _guard: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl CanonicalTempDir {
    fn new() -> Self {
        let guard = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(guard.path()).unwrap();
        Self {
            _guard: guard,
            path,
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn test_state(tmp_home: &std::path::Path) -> AppState {
    let root = tmp_home.join("demo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(tmp_home.join(".rtrt")) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(tmp_home.join(".rtrt"), permissions).unwrap();
    }
    let project = Arc::new(rtrt_core::ProjectIdentity::derive(&root).unwrap());
    *TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()) = project.slug().to_string();
    let mem_path = rtrt_core::project_memory_db_path_in(tmp_home, &project);
    let memory = Arc::new(Mutex::new(
        MemoryStore::open_project_in(&project, tmp_home).expect("open memory store"),
    ));
    let context = Arc::new(crate::state::ProjectContext::new(
        project.clone(),
        memory.clone(),
        mem_path.clone(),
    ));
    let catalog = Arc::new(crate::project_catalog::ProjectCatalog::from_context(
        tmp_home.to_path_buf(),
        context,
    ));
    let (events, _) = broadcast::channel::<String>(256);
    AppState {
        gateway: Arc::new(Gateway::from_env()),
        prompts: None,
        memory: Some(memory),
        auto_capture: false,
        auto_redact: false,
        project: project.clone(),
        session_id: "test-session".to_string(),
        dedup_window_sec: 0,
        events,
        embedder: None,
        cluster_cache: Arc::new(Mutex::new(HashMap::new())),
        brainh_cache: Arc::new(Mutex::new(HashMap::new())),
        level_tokens: Arc::new(Mutex::new(HashMap::new())),
        memory_path: mem_path,
        embedding_jobs: Arc::new(StdMutex::new(HashSet::new())),
        catalog,
        selected: true,
        daemon_projects: Arc::new(StdMutex::new(HashSet::from([project.slug().to_string()]))),
    }
}

/// Drive `app` with `req` via `oneshot`, unwrapping the `Infallible` result.
async fn call(app: axum::Router, req: Request<Body>) -> axum::response::Response {
    app.oneshot(req)
        .await
        .expect("axum router service is infallible")
}

fn get(uri: &str) -> Request<Body> {
    let slug = TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let uri = uri.replace("project=demo", &format!("project={slug}"));
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .header("X-RTRT-Project", slug)
        .body(Body::empty())
        .unwrap()
}

fn json(method: Method, uri: &str, body: &str) -> Request<Body> {
    let slug = TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let uri = uri.replace("project=demo", &format!("project={slug}"));
    let body = body.replace(r#""project":"demo""#, &format!(r#""project":"{slug}""#));
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "http://localhost:7311")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("X-RTRT-Project", slug)
        .body(Body::from(body))
        .unwrap()
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_body(resp: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_text(resp).await).unwrap()
}

#[test]
fn admin_scope_without_token_fails_closed() {
    let error = crate::validate_scope("admin", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires nonempty RTRT_DASHBOARD_TOKEN"));
    assert!(crate::validate_scope("admin", Some("secret")).is_err());
}

#[test]
fn startup_without_token_fails_closed() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    assert!(
        crate::dashboard_token()
            .unwrap_err()
            .to_string()
            .contains("requires nonempty RTRT_DASHBOARD_TOKEN")
    );
}

#[cfg(unix)]
#[test]
fn machine_startup_requires_exact_private_state_and_redacts_token() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state_dir = tmp.path().join(".rtrt/dashboard");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::set_permissions(
        tmp.path().join(".rtrt"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let env_file = state_dir.join("dashboard.env");
    std::fs::write(&env_file, "RTRT_DASHBOARD_TOKEN=machine-secret\n").unwrap();
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let startup = crate::MachineStartup::parse([
        "--machine".into(),
        "--state-dir".into(),
        state_dir.clone().into_os_string(),
    ])
    .unwrap();
    assert_eq!(startup.token, "machine-secret");

    let error = crate::MachineStartup::parse([
        "--machine".into(),
        "--state-dir".into(),
        tmp.path().join("other").into_os_string(),
    ])
    .unwrap_err()
    .to_string();
    assert!(!error.contains("machine-secret"));
}

#[tokio::test]
async fn project_bound_route_never_falls_back_without_selector() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let request = Request::builder()
        .uri("/api/memory/timeline")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        call(router(state, None), request).await.status(),
        StatusCode::BAD_REQUEST
    );
}

#[test]
fn production_dashboard_sources_do_not_mutate_environment() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|value| value.to_str()) != Some("rs")
            || entry.path().file_name().and_then(|value| value.to_str()) == Some("tests.rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(entry.path()).unwrap();
        assert!(
            !source.contains("env::set_var"),
            "{}",
            entry.path().display()
        );
        assert!(
            !source.contains("env::remove_var"),
            "{}",
            entry.path().display()
        );
    }
}

#[test]
fn ui_auth_uses_session_storage_and_one_retry() {
    let source = include_str!("../ui/assets/js/api.js");
    assert!(source.contains("sessionStorage.setItem(TOKEN_KEY"));
    assert!(source.contains("exactly one retry"));
    assert!(!source.contains("localStorage.setItem(TOKEN_KEY"));
    assert!(!source.contains("URLSearchParams"));
}

#[tokio::test]
async fn foreign_project_and_global_routes_are_denied() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    assert_eq!(
        call(app.clone(), get("/api/memory/timeline?project=foreign"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            app.clone(),
            get("/api/memory/graph?project=__global__&mode=brain")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(app, get("/api/projects/hidden")).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn bearer_api_mutation_without_origin_is_accepted() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/memory/save")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            "X-RTRT-Project",
            TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )
        .body(Body::from(r#"{"body":"x"}"#))
        .unwrap();
    assert_eq!(
        call(router(state, None), request).await.status(),
        StatusCode::OK
    );
}

#[cfg(unix)]
#[tokio::test]
async fn repo_map_rejects_outside_and_symlink_escape() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let outside = CanonicalTempDir::new();
    std::os::unix::fs::symlink(outside.path(), state.project.checkout_root().join("escape"))
        .unwrap();
    for root in [
        outside.path().to_string_lossy().into_owned(),
        "escape".into(),
    ] {
        let response = call(
            router(state.clone(), None),
            json(
                Method::POST,
                "/api/repo-map",
                &serde_json::json!({"root": root}).to_string(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn healthz_returns_ok() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/healthz")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_text(resp).await, "ok");
}

#[tokio::test]
async fn models_contract_preserves_unavailable_configured_compatible_model() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let path = config_file(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"[auto_compress]
model = "gemma3:4b-it-qat"
base_url = "http://127.0.0.1:9/v1"
"#,
    )
    .unwrap();
    let response = call(router(test_state(tmp.path()), None), get("/api/models")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let configured = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["source"] == "configured")
        .unwrap();
    assert_eq!(configured["id"], "openai-compat/gemma3:4b-it-qat");
    assert_eq!(configured["upstream_id"], "gemma3:4b-it-qat");
    assert_eq!(configured["provider"], "openai-compat");
    assert_eq!(configured["transport"], "openai-compatible");
    assert_eq!(configured["available"], false);
    assert_eq!(
        configured["label"],
        "gemma3:4b-it-qat — OpenAI-compatible endpoint"
    );
}

#[test]
fn ollama_model_dto_uses_canonical_id_and_honest_label() {
    let value = serde_json::to_value(crate::handlers::config::model_entry(
        "ollama",
        "gemma3:4b-it-qat",
        "local",
        true,
    ))
    .unwrap();
    assert_eq!(value["id"], "ollama/gemma3:4b-it-qat");
    assert_eq!(value["upstream_id"], "gemma3:4b-it-qat");
    assert_eq!(value["provider"], "ollama");
    assert_eq!(value["transport"], "openai-compatible");
    assert_eq!(value["source"], "local");
    assert_eq!(value["available"], true);
    assert_eq!(
        value["label"],
        "gemma3:4b-it-qat — Ollama (OpenAI-compatible)"
    );
}

#[test]
fn model_dto_normalizes_provider_aliases_without_losing_nested_model_ids() {
    for (provider, upstream, id, normalized, label) in [
        (
            "OLLAMA",
            "Ollama/org/model:tag",
            "ollama/org/model:tag",
            "ollama",
            "org/model:tag — Ollama (OpenAI-compatible)",
        ),
        (
            "openai-compatible",
            "vendor/model:1",
            "openai-compat/vendor/model:1",
            "openai-compat",
            "vendor/model:1 — OpenAI-compatible endpoint",
        ),
        (
            "LMStudio",
            "publisher/model",
            "lm-studio/publisher/model",
            "lm-studio",
            "publisher/model — LM Studio (OpenAI-compatible)",
        ),
        (
            "vLLM",
            "model:latest",
            "vllm/model:latest",
            "vllm",
            "model:latest — vLLM (OpenAI-compatible)",
        ),
        (
            "Azure-West",
            "deployment/family:model",
            "azure-west/deployment/family:model",
            "azure-west",
            "deployment/family:model — azure-west (OpenAI-compatible)",
        ),
    ] {
        let value = serde_json::to_value(crate::handlers::config::model_entry_with_transport(
            provider,
            upstream,
            "configured",
            false,
            provider == "Azure-West",
        ))
        .unwrap();
        assert_eq!(value["id"], id, "{provider}");
        assert_eq!(value["provider"], normalized, "{provider}");
        assert_eq!(value["label"], label, "{provider}");
        assert_eq!(value["available"], false, "{provider}");
    }
}

#[test]
fn unknown_provider_without_compatible_endpoint_is_not_given_a_protocol_identity() {
    let value = serde_json::to_value(crate::handlers::config::model_entry(
        "Google",
        "publisher/model:tag",
        "configured",
        false,
    ))
    .unwrap();
    assert_eq!(value["id"], "google/publisher/model:tag");
    assert_eq!(value["provider"], "google");
    assert_eq!(value["transport"], "unknown");
    assert_eq!(value["label"], "publisher/model:tag — google");
}

#[tokio::test]
async fn stats_returns_zeroed_json() {
    let tmp = CanonicalTempDir::new();
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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        let id = store
            .save(state.project.slug(), "note", "remember to ship the feature")
            .unwrap();
        store.tag_row(id, Some("sess-1"), Some("sha1")).unwrap();
    }
    let app = router(state.clone(), None);
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
    assert!(
        v.get("warning").is_none(),
        "healthy envelope stays compatible"
    );
}

#[tokio::test]
#[ignore = "legacy multi-project registry behavior removed by project isolation"]
async fn projects_survives_invalid_unrelated_team_config() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let config = config_file(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        config,
        r#"
[[projects]]
name = "registered"
path = "/safe/project"

[team]
enabled = true
leader_order = ["loop"]

[[team.members]]
name = "loop"
target = "claude"
mode = "cli"
roles = ["lead"]
fallback = ["loop"]
"#,
    )
    .unwrap();
    let state = test_state(tmp.path());
    state
        .memory
        .as_ref()
        .unwrap()
        .lock()
        .await
        .save("memory-only", "note", "still visible")
        .unwrap();

    let resp = call(router(state, None), get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let projects = v["projects"].as_array().unwrap();
    assert!(projects.iter().any(|p| p["name"] == "registered"));
    assert!(projects.iter().any(|p| p["name"] == "memory-only"));
    assert!(
        v["warning"]
            .as_str()
            .unwrap()
            .contains("Full configuration")
    );
    assert!(v["warning"].as_str().unwrap().len() <= 320);
}

#[tokio::test]
#[ignore = "legacy multi-project registry behavior removed by project isolation"]
async fn projects_filters_capture_buckets_when_registry_is_unavailable() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let config = config_file(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "not valid = [toml").unwrap();
    let state = test_state(tmp.path());
    let orphan = "agent-1234";
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        store.save(orphan, "note", "hidden capture").unwrap();
        store.save("visible", "note", "real project").unwrap();
    }

    let resp = call(router(state, None), get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    let projects = v["projects"].as_array().unwrap();
    assert!(projects.iter().all(|p| p["name"] != orphan));
    assert!(projects.iter().any(|p| p["name"] == "visible"));
    assert_eq!(v["hidden_capture_buckets"], 1);
    assert_eq!(v["hidden_capture_bucket_rows"], 1);
    assert!(
        v["warning"]
            .as_str()
            .unwrap()
            .contains("registry unavailable")
    );
}

#[tokio::test]
#[ignore = "legacy multi-project registry behavior removed by project isolation"]
async fn projects_memory_disabled_returns_config_only_with_warning() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let config = config_file(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[[projects]]\nname = \"registered\"\n").unwrap();
    let mut state = test_state(tmp.path());
    state.memory = None;

    let resp = call(router(state, None), get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["projects"][0]["name"], "registered");
    assert!(v["warning"].as_str().unwrap().contains("disabled"));
}

#[tokio::test]
#[ignore = "legacy multi-project registry behavior removed by project isolation"]
async fn projects_memory_query_error_returns_config_only_with_warning() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let config = config_file(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[[projects]]\nname = \"registered\"\n").unwrap();
    let state = test_state(tmp.path());
    rusqlite::Connection::open(tmp.path().join("memory.sqlite"))
        .unwrap()
        .execute("DROP TABLE memories", [])
        .unwrap();

    let resp = call(router(state, None), get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["projects"][0]["name"], "registered");
    assert!(v["warning"].as_str().unwrap().contains("query failed"));
}

#[tokio::test]
#[ignore = "legacy multi-project registry behavior removed by project isolation"]
async fn projects_both_sources_failed_returns_bounded_503() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let config = config_file(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "not valid = [toml").unwrap();
    let mut state = test_state(tmp.path());
    state.memory = None;

    let resp = call(router(state, None), get("/api/projects")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_text(resp).await;
    assert!(body.contains("configuration"));
    assert!(body.contains("memory"));
    assert!(body.len() <= 320);
    assert!(!body.contains(tmp.path().to_string_lossy().as_ref()));
}

/// A bucket named like a machine-generated session-hash pair (the confirmed
/// orphan shape: source transcript deleted, reattribution can never resolve
/// it) must not clutter the selector — but its rows stay in the store.
#[tokio::test]
#[ignore = "legacy hidden-bucket enumeration removed by project isolation"]
async fn projects_hides_orphan_capture_buckets_but_keeps_rows() {
    let tmp = CanonicalTempDir::new();
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
#[ignore = "legacy registry mutation removed by project isolation"]
async fn projects_registered_entry_is_never_hidden_even_if_name_matches() {
    let tmp = CanonicalTempDir::new();
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
#[ignore = "legacy project reassignment removed by project isolation"]
async fn projects_reassign_folds_orphan_rows_into_target() {
    let tmp = CanonicalTempDir::new();
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
#[ignore = "legacy project reassignment removed by project isolation"]
async fn projects_reassign_rejects_same_from_and_to() {
    let tmp = CanonicalTempDir::new();
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
    let tmp = CanonicalTempDir::new();
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
    assert_eq!(v["scope"], "custom");

    // GET reads the persisted global override back.
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/compression/config")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["level"], "lite");
    assert_eq!(v["enabled"], true);
    assert_eq!(v["scope"], "custom");

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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        let id = store
            .save(state.project.slug(), "note", "first row")
            .unwrap();
        store.tag_row(id, Some("sess-1"), Some("sha1")).unwrap();
        let id = store
            .save(state.project.slug(), "note", "second row")
            .unwrap();
        store.tag_row(id, Some("sess-1"), Some("sha2")).unwrap();
        let id = store
            .save(state.project.slug(), "note", "other session")
            .unwrap();
        store.tag_row(id, Some("sess-2"), Some("sha3")).unwrap();
    }
    let app = router(state.clone(), None);
    let resp = call(app, get("/api/memory/sessions?project=demo")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["project"], state.project.slug());
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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/api/memory/sessions?project=ghost")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `mode=overview` builds the whole-project LOD index and mints one drill
/// token per bubble — the live path the Memory map actually uses.
#[tokio::test]
async fn memory_graph_overview_returns_bubbles_with_tokens() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for i in 0..6 {
            store
                .save(
                    state.project.slug(),
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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for i in 0..6 {
            store
                .save(
                    state.project.slug(),
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
    let tmp = CanonicalTempDir::new();
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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let resp = call(app, get("/api/stats")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_text(resp).await;
    assert!(body.contains("unauthorized"), "{body}");
}

#[tokio::test]
async fn bearer_guard_advertises_challenge() {
    let tmp = CanonicalTempDir::new();
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
    let tmp = CanonicalTempDir::new();
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

    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let resp = call(app, get("/assets/js/api.js")).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn browser_origin_cannot_follow_attacker_host() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/stats")
        .header(header::HOST, "attacker.example")
        .header(header::ORIGIN, "http://attacker.example")
        .header(header::AUTHORIZATION, "Bearer s3cr3t")
        .header(
            "X-RTRT-Project",
            TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        call(router(test_state(tmp.path()), Some("s3cr3t".into())), req)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn explicit_bearer_client_without_origin_is_accepted() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/stats")
        .header(header::AUTHORIZATION, "Bearer s3cr3t")
        .header(
            "X-RTRT-Project",
            TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        call(router(state, Some("s3cr3t".into())), req)
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn bearer_guard_accepts_correct_token() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/stats")
        .header(header::AUTHORIZATION, "Bearer s3cr3t")
        .header(
            "X-RTRT-Project",
            TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )
        .body(Body::empty())
        .unwrap();
    let resp = call(app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn bearer_guard_rejects_wrong_token() {
    let tmp = CanonicalTempDir::new();
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

const BOOTSTRAP_TEST_TOKEN: &str =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn bootstrap_request(body: String, origin: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/bootstrap")
        .header(header::CONTENT_TYPE, "application/json");
    if origin {
        builder = builder.header(header::ORIGIN, "http://127.0.0.1:7311");
    }
    builder.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn bootstrap_exchange_requires_origin_but_not_bearer() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let credential =
        rtrt_core::dashboard_bootstrap::issue_with_nonce(BOOTSTRAP_TEST_TOKEN, now, 60, [9; 16])
            .unwrap();
    let body = serde_json::json!({ "credential": credential }).to_string();
    let app = router(
        test_state(tmp.path()),
        Some(BOOTSTRAP_TEST_TOKEN.to_string()),
    );
    let missing_origin = call(app.clone(), bootstrap_request(body.clone(), false)).await;
    assert_eq!(missing_origin.status(), StatusCode::FORBIDDEN);

    let success = call(app, bootstrap_request(body, true)).await;
    assert_eq!(success.status(), StatusCode::OK);
    assert_eq!(success.headers()[header::CACHE_CONTROL], "no-store");
    let payload = json_body(success).await;
    assert_eq!(payload["token"], BOOTSTRAP_TEST_TOKEN);
}

#[tokio::test]
async fn bootstrap_exchange_rejects_replay_malformed_and_oversized_generically() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let credential =
        rtrt_core::dashboard_bootstrap::issue_with_nonce(BOOTSTRAP_TEST_TOKEN, now, 60, [8; 16])
            .unwrap();
    let body = serde_json::json!({ "credential": credential }).to_string();
    let app = router(
        test_state(tmp.path()),
        Some(BOOTSTRAP_TEST_TOKEN.to_string()),
    );
    assert_eq!(
        call(app.clone(), bootstrap_request(body.clone(), true))
            .await
            .status(),
        StatusCode::OK
    );
    let replay = call(app.clone(), bootstrap_request(body, true)).await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(replay.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(body_text(replay).await, "bootstrap rejected");

    let malformed = call(app.clone(), bootstrap_request("{}".into(), true)).await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(malformed.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(body_text(malformed).await, "bootstrap rejected");
    let oversized = call(
        app,
        bootstrap_request(
            serde_json::json!({ "credential": "A".repeat(300) }).to_string(),
            true,
        ),
    )
    .await;
    assert_eq!(oversized.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_text(oversized).await, "bootstrap rejected");
}

#[test]
fn dashboard_js_clears_fragment_before_exchange_and_uses_session_storage_only() {
    let source = crate::assets::ASSET_JS_API;
    let clear = source.find("window.history.replaceState").unwrap();
    let exchange = source.find("nativeFetch('/api/auth/bootstrap'").unwrap();
    assert!(clear < exchange);
    assert!(source.contains("sessionStorage.setItem(TOKEN_KEY, payload.token)"));
    assert!(source.contains("sessionStorage.removeItem(TOKEN_KEY)"));
    let fragment_rejection = source.find("if (hadBootstrapFragment)").unwrap();
    let manual_fallback = source
        .find("const token = await requestTokenOnce()")
        .unwrap();
    assert!(fragment_rejection < manual_fallback);
    assert!(source.contains("window.prompt('Dashboard API token:')"));
    assert!(source.contains("window.dashboardAuthReady = bootstrapPromise"));
    assert!(!source.contains("localStorage.setItem(TOKEN_KEY"));
    assert!(!source.contains("document.cookie"));
}

#[test]
fn dashboard_app_waits_for_auth_and_direct_visits_keep_manual_fallback() {
    let auth = crate::assets::ASSET_JS_API;
    let app = crate::assets::ASSET_JS_APP;
    assert!(auth.contains("if (!hadBootstrapFragment) return true"));
    assert!(auth.contains("const token = await requestTokenOnce()"));
    let gate = app.find("window.dashboardAuthReady.then").unwrap();
    let init = app.find("syncOverviewWindowButtons();").unwrap();
    assert!(gate < init);
    let html = crate::assets::INDEX_HTML;
    assert!(html.find("/assets/js/api.js").unwrap() < html.find("/assets/js/app.js").unwrap());
}

#[tokio::test]
async fn bootstrap_shell_and_unhashed_assets_are_never_stored() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), Some("s3cr3t".to_string()));
    for path in [
        "/",
        "/memory/search",
        "/assets/styles.css",
        "/assets/js/api.js",
    ] {
        let response = call(app.clone(), get(path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store",
            "{path}"
        );
    }
}

#[tokio::test]
async fn spa_fallback_serves_deep_path_as_html() {
    let tmp = CanonicalTempDir::new();
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
    let tmp = CanonicalTempDir::new();
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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    const TOTAL: usize = 300;
    const SESSIONS: usize = 5;
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        for i in 0..TOTAL {
            let body = format!("uniqueword{i}xyz alphatok{i}abc betatok{i}def gammatok{i}ghi");
            let id = store.save(state.project.slug(), "note", &body).unwrap();
            let session = format!("sess-{}", i % SESSIONS);
            store.tag_row(id, Some(&session), None).unwrap();
        }
    }
    let app = router(state.clone(), None);
    let resp = call(
        app,
        get("/api/memory/graph?project=demo&mode=overview&group=context&basis=lexical"),
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
    let tmp = CanonicalTempDir::new();
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
            store
                .save(state.project.slug(), kind, "typed by the user")
                .unwrap();
        }
        for kind in output_kinds {
            store
                .save(state.project.slug(), kind, "produced by an agent")
                .unwrap();
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
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    {
        let store = state.memory.as_ref().unwrap().lock().await;
        store
            .save(
                state.project.slug(),
                "user-prompt-submit",
                "fix the parser bug",
            )
            .unwrap();
        store
            .save(
                state.project.slug(),
                "assistant-turn",
                "fixed the parser bug",
            )
            .unwrap();
        store
            .save(
                state.project.slug(),
                "teammate-message",
                "parser bug report",
            )
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
// Failover config — `[failover]` markers. Native team/roster endpoints are gone.
// ---------------------------------------------------------------------------

/// A POST with no body — the "Follow global" clear path every scoped endpoint
/// exposes. Does **not** inject a project selector.
fn post_empty(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::ORIGIN, "http://localhost:7311")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn test_slug() -> String {
    TEST_SLUG.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn unselected(mut state: AppState) -> AppState {
    state.selected = false;
    state
}

/// GET without the `X-RTRT-Project` header `get()` always injects.
fn bare_get(uri: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

/// JSON POST/PUT without the `X-RTRT-Project` header `json()` always injects.
fn bare_json(method: Method, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost")
        .header(header::ORIGIN, "http://localhost:7311")
        .header(header::AUTHORIZATION, format!("Bearer {TEST_TOKEN}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn with_project_header(mut req: Request<Body>, slug: &str) -> Request<Body> {
    req.headers_mut().insert(
        "X-RTRT-Project",
        axum::http::HeaderValue::from_str(slug).expect("slug is a valid header value"),
    );
    req
}

async fn failover_handler_get(state: AppState) -> axum::response::Response {
    crate::handlers::failover::get_failover_config(
        axum::Extension(state),
        axum::extract::Query(crate::handlers::scope::ProjectQuery::default()),
    )
    .await
}

async fn failover_handler_post(
    state: AppState,
    scope: Option<&str>,
    body: Option<&str>,
) -> axum::response::Response {
    let q = crate::handlers::scope::ProjectQuery {
        project: None,
        scope: scope.map(str::to_string),
    };
    let parsed = body.map(|raw| {
        axum::Json(
            serde_json::from_str::<crate::handlers::failover::SetFailoverRequest>(raw)
                .expect("test failover body"),
        )
    });
    crate::handlers::failover::post_failover_config(
        axum::Extension(state),
        axum::extract::Query(q),
        parsed,
    )
    .await
}

#[tokio::test]
async fn team_config_api_is_unavailable() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    let app = router(state.clone(), None);
    let resp = call(app, get("/api/team/config")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let app = router(state, None);
    let resp = call(
        app,
        json(Method::POST, "/api/team/config", r#"{"enabled":true}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn failover_global_get_post_roundtrip() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let repo = state.project.memory_root().to_path_buf();
    let app = router(unselected(state.clone()), None);

    let resp = call(app, bare_get("/api/failover/config")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["custom"], false);
    assert_eq!(v["inherited"], false);
    assert!(v["fatal"].as_array().expect("fatal").is_empty());
    assert!(v["transient_retries"].is_null());

    let app = router(unselected(state.clone()), None);
    let resp = call(
        app,
        bare_json(
            Method::POST,
            "/api/failover/config",
            r#"{"fatal":["contract expired"," "],"quota":["seat limit reached"],
                "transient":[],"transient_retries":1,"backoff_divisor":60,"backoff_ms":null}"#,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], false);
    assert_eq!(v["fatal"].as_array().expect("fatal").len(), 1);
    assert_eq!(v["fatal"][0], "contract expired");
    assert_eq!(v["quota"][0], "seat limit reached");
    assert_eq!(v["transient_retries"], 1);
    assert_eq!(v["backoff_divisor"], 60);
    assert!(v["backoff_ms"].is_null());
    assert!(!repo.join(".rtrt").join("config.toml").exists());

    let app = router(unselected(state), None);
    let resp = call(app, bare_get("/api/failover/config")).await;
    let v = json_body(resp).await;
    assert_eq!(v["fatal"][0], "contract expired");
    assert_eq!(v["transient_retries"], 1);
}

#[tokio::test]
async fn failover_inherited_project_get_shows_global_policy() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let slug = test_slug();

    let resp = failover_handler_post(
        unselected(state.clone()),
        None,
        Some(r#"{"fatal":["global marker"],"quota":[],"transient":[],"transient_retries":3}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = call(
        router(state, None),
        with_project_header(bare_get("/api/failover/config"), &slug),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["custom"], false);
    assert_eq!(v["inherited"], true);
    assert_eq!(v["fatal"][0], "global marker");
    assert_eq!(v["transient_retries"], 3);
}

#[tokio::test]
async fn failover_project_custom_write_preserves_global() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let slug = test_slug();
    let repo = state.project.memory_root().to_path_buf();

    let resp = failover_handler_post(
        unselected(state.clone()),
        None,
        Some(r#"{"fatal":["global marker"],"quota":[],"transient":[],"transient_retries":3}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = call(
        router(state.clone(), None),
        with_project_header(
            bare_json(
                Method::POST,
                "/api/failover/config?scope=custom",
                r#"{"fatal":["project marker"],"quota":[],"transient":[],"transient_retries":null}"#,
            ),
            &slug,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");
    assert_eq!(v["custom"], true);
    assert_eq!(v["inherited"], false);
    assert_eq!(v["fatal"][0], "project marker");
    assert!(v["transient_retries"].is_null());
    assert!(repo.join(".rtrt").join("config.toml").exists());

    let resp = call(
        router(state.clone(), None),
        with_project_header(bare_get("/api/failover/config"), &slug),
    )
    .await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");
    assert_eq!(v["fatal"].as_array().expect("fatal").len(), 1);
    assert_eq!(v["fatal"][0], "project marker");

    let resp = failover_handler_get(unselected(state)).await;
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], false);
    assert_eq!(v["fatal"][0], "global marker");
    assert_eq!(v["transient_retries"], 3);
}

#[tokio::test]
async fn failover_project_follow_global_preserves_unrelated_overrides() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let slug = test_slug();
    let repo = state.project.memory_root().to_path_buf();

    let resp = failover_handler_post(
        unselected(state.clone()),
        None,
        Some(r#"{"fatal":["global marker"],"quota":[],"transient":[],"transient_retries":3}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let project = rtrt_core::config::ProjectConfig {
        output_level: Some("ultra".into()),
        failover: Some(rtrt_core::config::FailoverConfig {
            fatal: vec!["project marker".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    crate::util::write_project_config(&repo, &project).unwrap();

    let resp = call(
        router(state.clone(), None),
        with_project_header(post_empty("/api/failover/config?scope=global"), &slug),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "global");
    assert_eq!(v["inherited"], true);
    assert_eq!(v["fatal"][0], "global marker");

    let remaining = rtrt_core::Config::load_project(&repo).unwrap();
    assert!(remaining.failover.is_none());
    assert_eq!(remaining.output_level.as_deref(), Some("ultra"));

    let resp = failover_handler_get(unselected(state)).await;
    let v = json_body(resp).await;
    assert_eq!(v["fatal"][0], "global marker");
    assert_eq!(v["transient_retries"], 3);
}

#[tokio::test]
async fn failover_project_post_rejects_missing_and_invalid_scope() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let slug = test_slug();
    let repo = state.project.memory_root().to_path_buf();
    let body = r#"{"fatal":["should not persist"],"quota":[],"transient":[]}"#;

    let resp = call(
        router(state.clone(), None),
        with_project_header(bare_json(Method::POST, "/api/failover/config", body), &slug),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    assert!(
        v["error"]
            .as_str()
            .expect("error")
            .contains("scope=custom or scope=global")
    );

    let resp = call(
        router(state.clone(), None),
        with_project_header(
            bare_json(Method::POST, "/api/failover/config?scope=nope", body),
            &slug,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    assert!(
        v["error"]
            .as_str()
            .expect("error")
            .contains("invalid failover scope")
    );

    assert!(!repo.join(".rtrt").join("config.toml").exists());
    assert!(!config_file(tmp.path()).exists());
}

#[tokio::test]
async fn failover_numeric_fields_reject_values_above_u32_max() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    let at_max = format!(
        r#"{{"fatal":[],"quota":[],"transient":[],"transient_retries":{},"backoff_divisor":{}}}"#,
        u32::MAX,
        u32::MAX
    );
    let resp = call(
        router(unselected(state.clone()), None),
        bare_json(Method::POST, "/api/failover/config", &at_max),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["transient_retries"], u32::MAX);
    assert_eq!(v["backoff_divisor"], u32::MAX);

    let over_max = format!(
        r#"{{"fatal":[],"quota":[],"transient":[],"transient_retries":{}}}"#,
        u64::from(u32::MAX) + 1
    );
    let resp = call(
        router(unselected(state.clone()), None),
        bare_json(Method::POST, "/api/failover/config", &over_max),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    assert!(
        v["error"]
            .as_str()
            .expect("error")
            .contains("failover.transient_retries")
    );

    let resp = call(
        router(unselected(state), None),
        bare_get("/api/failover/config"),
    )
    .await;
    let v = json_body(resp).await;
    assert_eq!(v["transient_retries"], u32::MAX);
}

#[tokio::test]
async fn failover_header_only_project_selection() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let slug = test_slug();

    let resp = failover_handler_post(
        unselected(state.clone()),
        None,
        Some(r#"{"fatal":["global marker"],"quota":[],"transient":[],"transient_retries":3}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = call(
        router(state.clone(), None),
        with_project_header(bare_get("/api/failover/config"), &slug),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["inherited"], true);
    assert_eq!(v["fatal"][0], "global marker");

    let resp = call(
        router(state.clone(), None),
        with_project_header(
            bare_json(
                Method::POST,
                "/api/failover/config?scope=custom",
                r#"{"fatal":["header project"],"quota":[],"transient":[]}"#,
            ),
            &slug,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = json_body(resp).await;
    assert_eq!(v["scope"], "custom");
    assert_eq!(v["fatal"][0], "header project");

    let resp = failover_handler_get(unselected(state)).await;
    let v = json_body(resp).await;
    assert_eq!(v["fatal"][0], "global marker");
}

#[tokio::test]
async fn failover_rejects_zero_backoff_divisor() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());
    let slug = test_slug();
    let repo = state.project.memory_root().to_path_buf();
    let body = r#"{"fatal":[],"quota":[],"transient":[],"backoff_divisor":0}"#;

    let resp = failover_handler_post(unselected(state.clone()), None, Some(body)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = json_body(resp).await;
    assert!(
        v["error"]
            .as_str()
            .expect("error message")
            .contains("backoff_divisor")
    );
    assert!(!config_file(tmp.path()).exists());

    let resp = call(
        router(state, None),
        with_project_header(
            bare_json(Method::POST, "/api/failover/config?scope=custom", body),
            &slug,
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
    assert!(!repo.join(".rtrt").join("config.toml").exists());
}

#[tokio::test]
async fn config_patch_omissions_preserve_every_hidden_setting() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    std::fs::create_dir_all(config_file(tmp.path()).parent().unwrap()).unwrap();
    std::fs::write(
        config_file(tmp.path()),
        r#"
[dashboard]
bind = "0.0.0.0:9000"
[providers]
api_max_tokens = 777
[capture]
enabled = false
redact = false
dedup_window_sec = 987
project = "pinned"
[auto_compress]
enabled = true
model = "kept/model"
base_url = "http://kept"
provider = "kept-runtime"
interval_sec = 91
age_sec = 92
min_chars = 93
batch = 94
max_tokens = 95
[embeddings]
enabled = true
model = "kept-embed"
base_url = "http://embed"
auto = false
auto_interval_sec = 96
auto_batch = 97
"#,
    )
    .unwrap();
    let app = router(test_state(tmp.path()), None);
    let response = call(app, json(Method::POST, "/api/config", r#"{"capture":{"enabled":true},"auto_compress":{"age_sec":100},"embeddings":{"enabled":false}}"#)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cfg = rtrt_core::Config::load().unwrap();
    assert!(cfg.capture.enabled);
    assert!(!cfg.capture.redact);
    assert_eq!(cfg.capture.dedup_window_sec, 987);
    assert_eq!(cfg.capture.project.as_deref(), Some("pinned"));
    assert_eq!(cfg.auto_compress.interval_sec, 91);
    assert_eq!(cfg.auto_compress.age_sec, 100);
    assert_eq!(cfg.auto_compress.min_chars, 93);
    assert_eq!(cfg.auto_compress.batch, 94);
    assert_eq!(cfg.auto_compress.max_tokens, 95);
    assert_eq!(cfg.auto_compress.provider.as_deref(), Some("kept-runtime"));
    assert!(!cfg.embeddings.enabled);
    assert!(!cfg.embeddings.auto);
    assert_eq!(cfg.embeddings.model, "kept-embed");
    assert_eq!(cfg.embeddings.auto_interval_sec, 96);
    assert_eq!(cfg.embeddings.auto_batch, 97);
    assert_eq!(cfg.dashboard.bind, "0.0.0.0:9000");
    assert_eq!(cfg.providers.api_max_tokens, Some(777));
}

#[tokio::test]
#[ignore = "legacy project-registry mutation removed by project isolation"]
async fn project_patch_omissions_preserve_path_security_and_embedding() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let first = r#"{"name":"demo","path":"/kept/path","security_profile":"ai-strict","embeddings_mode":"off"}"#;
    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(Method::PUT, "/api/projects", first)
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(Method::PUT, "/api/projects", r#"{"name":"demo"}"#)
        )
        .await
        .status(),
        StatusCode::OK
    );
    let p = rtrt_core::Config::load()
        .unwrap()
        .project("demo")
        .unwrap()
        .clone();
    assert_eq!(p.path.as_deref(), Some("/kept/path"));
    assert_eq!(p.security_profile.as_deref(), Some("ai-strict"));
    assert_eq!(p.embeddings_enabled, Some(false));

    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(
                Method::PUT,
                "/api/projects",
                r#"{"name":"demo","path":null}"#,
            )
        )
        .await
        .status(),
        StatusCode::OK
    );
    let p = rtrt_core::Config::load()
        .unwrap()
        .project("demo")
        .unwrap()
        .clone();
    assert_eq!(p.path, None);
    assert_eq!(p.security_profile.as_deref(), Some("ai-strict"));
    assert_eq!(p.embeddings_enabled, Some(false));
}

#[tokio::test]
async fn limits_patch_omissions_preserve_axes_and_pool_only_limits() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let initial = r#"{"targets":[{"target":"api","daily_tokens":10,"daily_requests":20,"pools":[{"pool":"paid","daily_tokens":30}]}]}"#;
    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(Method::POST, "/api/limits/config", initial)
        )
        .await
        .status(),
        StatusCode::OK
    );
    let patch = r#"{"targets":[{"target":"api","daily_tokens":11}]}"#;
    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(Method::POST, "/api/limits/config", patch)
        )
        .await
        .status(),
        StatusCode::OK
    );
    let limit = rtrt_core::Config::load()
        .unwrap()
        .limits
        .target("api")
        .unwrap()
        .clone();
    assert_eq!(limit.daily_tokens, Some(11));
    assert_eq!(limit.daily_requests, Some(20));
    assert_eq!(limit.pools["paid"].daily_tokens, Some(30));

    let clear = r#"{"targets":[{"target":"api","daily_requests":null}]}"#;
    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(Method::POST, "/api/limits/config", clear)
        )
        .await
        .status(),
        StatusCode::OK
    );
    let limit = rtrt_core::Config::load()
        .unwrap()
        .limits
        .target("api")
        .unwrap()
        .clone();
    assert_eq!(limit.daily_tokens, Some(11));
    assert_eq!(limit.daily_requests, None);
    assert_eq!(limit.pools["paid"].daily_tokens, Some(30));
}

#[tokio::test]
async fn security_profile_clone_payload_preserves_full_schema() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let toml = r#"name = "complete"
description = "all fields"
severity_threshold = "medium"
exclude = ["vendor/"]

[[rules]]
id = "pattern.complete"
engine = "patterns"
severity = "high"
description = "kept"
enabled = false
standards = { cwe = ["CWE-78"], eu_ai_act = ["Art.15"] }
match = "danger"
langs = ["rs", "js"]
"#;
    let body = serde_json::json!({"name": "complete", "toml": toml}).to_string();
    let response = call(
        router(test_state(tmp.path()), None),
        json(Method::POST, "/api/security/profile", &body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = call(
        router(test_state(tmp.path()), None),
        get("/api/security/profile/complete"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value = json_body(response).await;
    assert_eq!(value["profile"]["rules"][0]["enabled"], false);
    assert_eq!(
        value["profile"]["rules"][0]["standards"]["cwe"][0],
        "CWE-78"
    );
    assert_eq!(value["profile"]["rules"][0]["match"], "danger");
    assert!(value["toml"].as_str().unwrap().contains("eu_ai_act"));
}

#[tokio::test]
async fn security_profile_rejects_traversal_and_name_mismatch() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let bad = r#"{"name":"../escape","toml":"name = 'escape'\n"}"#;
    assert_eq!(
        call(app, json(Method::POST, "/api/security/profile", bad))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mismatch = r#"{"name":"safe","toml":"name = 'other'\n"}"#;
    assert_eq!(
        call(
            router(test_state(tmp.path()), None),
            json(Method::POST, "/api/security/profile", mismatch)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
}

#[cfg(unix)]
#[tokio::test]
async fn config_writer_rejects_symlink_target_without_touching_destination() {
    use std::os::unix::fs::symlink;
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    std::fs::create_dir_all(config_file(tmp.path()).parent().unwrap()).unwrap();
    let destination = tmp.path().join("victim");
    std::fs::write(&destination, "keep").unwrap();
    symlink(&destination, config_file(tmp.path())).unwrap();
    let response = call(
        router(test_state(tmp.path()), None),
        json(
            Method::POST,
            "/api/config",
            r#"{"capture":{"enabled":false}}"#,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(std::fs::read_to_string(destination).unwrap(), "keep");
}

#[tokio::test]
async fn failover_page_assets_are_served_and_deep_linkable() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let state = test_state(tmp.path());

    // The page's script is embedded in the binary and served like the other
    // app assets: `no-store`, so auth/bootstrap code is never restored stale.
    let app = router(state.clone(), None);
    let resp = call(app, get("/assets/js/failover.js")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let script = body_text(resp).await;
    assert!(script.contains("loadFailover"));

    // The shell loads it, and the page + nav entry exist in the markup.
    let app = router(state.clone(), None);
    let html = body_text(call(app, get("/")).await).await;
    assert!(html.contains("/assets/js/failover.js"));
    assert!(html.contains("id=\"page-failover\""));
    assert!(html.contains("data-page=\"failover\""));

    // Deep link: /failover falls through to the SPA shell so a refresh
    // (or a shared URL) restores the page instead of 404ing.
    let app = router(state, None);
    let resp = call(app, get("/failover")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/html")
    );
}

#[tokio::test]
async fn retired_orchestration_asset_is_not_served() {
    let tmp = CanonicalTempDir::new();
    let _g = EnvGuard::new(tmp.path());
    let app = router(test_state(tmp.path()), None);
    let resp = call(app, get("/assets/js/orchestration.js")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
