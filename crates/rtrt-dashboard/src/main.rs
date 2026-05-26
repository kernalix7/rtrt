//! rtrt-dashboard — axum web UI + REST API.
//!
//! Surfaces:
//! - `/`               — bundled HTML index (mini-app: stats / templates / metrics).
//! - `/healthz`        — liveness probe.
//! - `/api/stats`      — compression / proxy savings JSON.
//! - `/api/templates`  — list built-in + custom templates.
//! - `/api/templates/{name}` — full manifest for one template.
//! - `/api/templates/scaffold` — `POST` scaffold a project.
//! - `/api/chat`       — `POST` chat through the bundled provider gateway.
//! - `/api/metrics`    — gateway summary + recent per-request metrics.
//! - `/api/prompts`    — list versioned prompts from the langfuse-style registry.
//! - `/api/prompts/{name}` — list versions for a single prompt.
//! - `/api/prompts/{name}/{version}` — full prompt body.
//! - `/api/budget`     — gateway budget cap + cumulative spend.
//! - `/api/memory/recall` — `POST` BM25 recall with optional qdrant-style payload filter.
//! - `/api/memory/stats`  — `GET` aggregate stats for a project (total, by_kind, compressed).
//! - `/api/memory/save`   — `POST` insert a memory row with optional metadata.
//! - `/api/memory/blocks` — `GET` list / `POST` set Letta-style memory blocks.
//! - `/api/memory/blocks/{name}` — `GET` a single block (project as query param).
//! - `/api/compress`      — `POST` run the rule, ML, or LLM compressor against arbitrary text.
//! - `/api/proxy`         — `POST` rtrt-proxy filters (command / errors_only / ultra_compact).
//! - `/api/diagnose`      — `POST` aider-style failure triage (errors_only + gateway chat).
//!
//! All `/api/*` routes are gated by a bearer-token middleware when the
//! `RTRT_DASHBOARD_TOKEN` env var is set; the bundled HTML index and the
//! `/healthz` probe remain open so the UI can bootstrap.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use rtrt_memory::{MemoryStore, PayloadFilter};
use rtrt_providers::{ChatMessage, ChatRequest, Gateway, MetricsView, RequestMetric, Role};
use rtrt_templates::{Prompt, PromptRegistry};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::broadcast;

#[derive(Clone)]
struct AppState {
    gateway: Arc<Gateway>,
    prompts: Option<Arc<PromptRegistry>>,
    memory: Option<Arc<Mutex<MemoryStore>>>,
    auto_capture: bool,
    auto_redact: bool,
    default_project: String,
    session_id: String,
    dedup_window_sec: i64,
    events: broadcast::Sender<String>,
}

fn broadcast_event(tx: &broadcast::Sender<String>, payload: serde_json::Value) {
    let _ = tx.send(payload.to_string());
}

impl AppState {
    /// Best-effort auto-save into the memory store. Pipeline:
    /// 1. Privacy filter (`redact_secrets`) when `auto_redact` is true.
    /// 2. SHA-256 dedup against the last `dedup_window_sec` seconds.
    /// 3. Insert raw row, then tag it with session_id + body_sha.
    async fn capture(
        &self,
        kind: &str,
        body: &str,
        metadata: &std::collections::BTreeMap<String, String>,
    ) {
        if !self.auto_capture {
            return;
        }
        let Some(store) = &self.memory else { return };
        let filtered = if self.auto_redact {
            rtrt_compress::redact_secrets(body)
        } else {
            body.to_string()
        };
        let project = self.default_project.clone();
        let kind = kind.to_string();
        let metadata = metadata.clone();
        let session = self.session_id.clone();
        let window = self.dedup_window_sec;
        let guard = store.lock().await;
        let sha = rtrt_memory::MemoryStore::body_sha(&filtered);
        if window > 0
            && let Ok(Some(last_ts)) = guard.body_seen_at(&project, &sha)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if now.saturating_sub(last_ts) < window {
                tracing::debug!(kind = %kind, "auto-capture deduped within {window}s window");
                return;
            }
        }
        let result = if metadata.is_empty() {
            guard.save(&project, &kind, &filtered)
        } else {
            guard.save_with_metadata(&project, &kind, &filtered, &metadata)
        };
        match result {
            Ok(id) => {
                if let Err(e) = guard.tag_row(id, Some(&session), Some(&sha)) {
                    tracing::warn!("auto-capture tag {kind}: {e}");
                }
                broadcast_event(
                    &self.events,
                    serde_json::json!({
                        "type": "memory.save",
                        "id": id,
                        "kind": kind,
                        "project": project,
                        "session": session,
                    }),
                );
            }
            Err(e) => tracing::warn!("auto-capture {kind}: {e}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info,tower_http=info")
        .init();

    let bind =
        std::env::var("RTRT_DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:7311".to_string());
    let token = std::env::var("RTRT_DASHBOARD_TOKEN").ok();
    if token.is_none()
        && !bind.starts_with("127.")
        && !bind.starts_with("[::1]")
        && !bind.starts_with("localhost")
    {
        tracing::warn!(
            "binding {bind} without RTRT_DASHBOARD_TOKEN is risky; non-loopback callers can hit the API without authentication."
        );
    }
    // Feed the config's auto_compress.base_url into the env before building
    // the gateway, so `Gateway::from_env` registers the local/OpenAI-compat
    // provider even when only ~/.rtrt/config.toml (not an env var) sets it.
    // Without this, the LLM compress engine + auto-compress daemon can't
    // route a local model like gemma3:4b.
    if std::env::var_os("RTRT_PROVIDER_BASE_URL").is_none()
        && std::env::var_os("RTRT_OPENAI_COMPAT_URL").is_none()
        && let Ok(cfg) = rtrt_core::Config::load()
        && let Some(url) = cfg.auto_compress.base_url.as_deref()
    {
        // SAFETY: set during single-threaded startup before any task spawns.
        unsafe { std::env::set_var("RTRT_PROVIDER_BASE_URL", url) };
    }
    let gateway = Arc::new(Gateway::from_env());
    let prompts = open_prompt_registry();
    let memory = open_memory_store();
    let auto_capture = std::env::var("RTRT_AUTO_CAPTURE")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);
    let auto_redact = std::env::var("RTRT_AUTO_REDACT")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);
    let dedup_window_sec: i64 = std::env::var("RTRT_AUTO_DEDUP_WINDOW_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let default_project =
        std::env::var("RTRT_DEFAULT_PROJECT").unwrap_or_else(|_| "default".into());
    let session_id = uuid::Uuid::new_v4().to_string();
    if auto_capture {
        tracing::info!(
            "auto-capture on (project={default_project}, redact={auto_redact}, dedup_window={dedup_window_sec}s, session={session_id})"
        );
    } else {
        tracing::info!("auto-capture off (RTRT_AUTO_CAPTURE=0)");
    }
    let memory_for_daemon = memory.clone();
    let memory_for_compress_daemon = memory.clone();
    let gateway_for_compress_daemon = gateway.clone();
    let (events_tx, _) = broadcast::channel::<String>(256);
    let state = AppState {
        gateway,
        prompts,
        memory,
        auto_capture,
        auto_redact,
        default_project,
        session_id,
        dedup_window_sec,
        events: events_tx,
    };
    spawn_consolidation_daemon(memory_for_daemon);
    spawn_auto_compress_daemon(memory_for_compress_daemon, gateway_for_compress_daemon);

    let token_arc = token.clone().map(Arc::new);
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/stats", get(stats))
        .route("/api/templates", get(list_templates))
        .route("/api/templates/{name}", get(get_template))
        .route("/api/templates/scaffold", post(scaffold))
        .route("/api/templates/scaffold/preview", post(scaffold_preview))
        .route("/api/chat", post(chat))
        .route("/api/metrics", get(metrics))
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts/{name}", get(list_prompt_versions))
        .route("/api/prompts/{name}/{version}", get(get_prompt))
        .route("/api/budget", get(budget))
        .route("/api/memory/projects", get(memory_projects))
        .route("/api/memory/timeline", get(memory_timeline))
        .route("/api/memory/recall", post(memory_recall))
        .route("/api/memory/graph", get(memory_graph))
        .route("/api/memory/export", get(memory_export))
        .route("/api/memory/save", post(memory_save))
        .route("/api/memory/blocks", get(list_blocks).post(set_block))
        .route("/api/memory/blocks/{name}", get(get_block))
        .route("/api/compress", post(compress))
        .route("/api/proxy", post(proxy_filter))
        .route("/api/diagnose", post(diagnose))
        .route("/api/repo-map", post(repo_map))
        .route("/api/setup", post(setup_snippet))
        .route("/api/stream", get(sse_stream))
        .route("/api/tokens/summary", get(tokens_summary))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/models", get(get_models))
        .route("/api/memory/compress", post(memory_compress))
        .route("/api/memory/stats", get(memory_stats))
        .layer(axum::middleware::from_fn(move |req, next| {
            let token = token_arc.clone();
            async move { bearer_guard(token, req, next).await }
        }))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "address {bind} is already in use. Free the port (lsof -i :{port}) or set RTRT_DASHBOARD_BIND to another address (e.g. RTRT_DASHBOARD_BIND=127.0.0.1:3211 rtrt-dashboard).",
                port = bind.rsplit(':').next().unwrap_or("7311"),
            );
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!("rtrt-dashboard listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Hourly background sweep — keeps each project's row count under
/// `RTRT_CONSOLIDATE_KEEP` (default 1000) using the LLM-free archive path.
/// Disabled when `RTRT_CONSOLIDATE_INTERVAL_SEC=0`.
fn spawn_consolidation_daemon(memory: Option<Arc<Mutex<MemoryStore>>>) {
    let interval_sec: u64 = std::env::var("RTRT_CONSOLIDATE_INTERVAL_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    if interval_sec == 0 {
        tracing::info!("consolidation daemon off (RTRT_CONSOLIDATE_INTERVAL_SEC=0)");
        return;
    }
    let keep: usize = std::env::var("RTRT_CONSOLIDATE_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let Some(store) = memory else {
        tracing::info!("consolidation daemon off (no memory store)");
        return;
    };
    tracing::info!(
        "consolidation daemon every {interval_sec}s, keep {keep} most-recent rows per project"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_sec));
        // The first tick fires immediately — skip it so the daemon doesn't
        // sweep on startup before any rows have accumulated.
        tick.tick().await;
        loop {
            tick.tick().await;
            let guard = store.lock().await;
            let projects = match guard.projects() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("consolidate: list projects: {e}");
                    continue;
                }
            };
            for (project, count, _) in projects {
                if count <= keep {
                    continue;
                }
                match guard.archive_overflow_no_llm(&project, keep) {
                    Ok(removed) if removed > 0 => {
                        tracing::info!(project = %project, removed, kept = keep, "consolidated");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("consolidate {project}: {e}"),
                }
            }
        }
    });
}

/// Opt-in background worker — sweeps memory rows older than
/// `RTRT_AUTO_COMPRESS_AGE_SEC` whose body is longer than
/// `RTRT_AUTO_COMPRESS_MIN_CHARS`, asks the configured LLM model to
/// compress each one, and writes the result back via `set_body`.
/// Idempotent: every rewritten row is tagged `metadata.compressed_at`,
/// and `compress_candidates` filters those out next sweep.
///
/// Disabled unless `RTRT_AUTO_COMPRESS_LLM=1`. Honours the same
/// `RTRT_AUTO_COMPRESS_*` knobs documented in `docs/USAGE.md`.
fn spawn_auto_compress_daemon(memory: Option<Arc<Mutex<MemoryStore>>>, gateway: Arc<Gateway>) {
    let enabled = std::env::var("RTRT_AUTO_COMPRESS_LLM")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
        .unwrap_or(false);
    if !enabled {
        tracing::info!("auto-compress daemon off (set RTRT_AUTO_COMPRESS_LLM=1 to enable)");
        return;
    }
    let Some(store) = memory else {
        tracing::info!("auto-compress daemon off (no memory store)");
        return;
    };
    let interval_sec: u64 = std::env::var("RTRT_AUTO_COMPRESS_INTERVAL_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800);
    let age_sec: i64 = std::env::var("RTRT_AUTO_COMPRESS_AGE_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let min_chars: usize = std::env::var("RTRT_AUTO_COMPRESS_MIN_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let batch: usize = std::env::var("RTRT_AUTO_COMPRESS_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let model = std::env::var("RTRT_AUTO_COMPRESS_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5".to_string());
    let max_tokens: u32 = std::env::var("RTRT_AUTO_COMPRESS_MAX_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    tracing::info!(
        "auto-compress daemon on: model={model}, every {interval_sec}s, age>{age_sec}s, min_chars={min_chars}, batch={batch}"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_sec));
        tick.tick().await;
        loop {
            tick.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let cutoff = now - age_sec;
            let projects = {
                let guard = store.lock().await;
                match guard.projects() {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("auto-compress: list projects: {e}");
                        continue;
                    }
                }
            };
            for (project, _, _) in projects {
                let candidates = {
                    let guard = store.lock().await;
                    match guard.compress_candidates(&project, cutoff, min_chars, batch) {
                        Ok(rows) => rows,
                        Err(e) => {
                            tracing::warn!("auto-compress {project}: candidates: {e}");
                            continue;
                        }
                    }
                };
                for (id, body) in candidates {
                    let req = ChatRequest {
                        model: model.clone(),
                        messages: vec![
                            ChatMessage {
                                role: Role::System,
                                content: "You are a lossless-meaning compressor. Rewrite the user message in the shortest form that preserves every fact, decision, file path, identifier, command, and number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary, no preamble, no quotes — emit only the compressed text.".to_string(),
                            },
                            ChatMessage {
                                role: Role::User,
                                content: body.clone(),
                            },
                        ],
                        max_tokens: Some(max_tokens),
                        temperature: Some(0.0),
                    };
                    let resp = match gateway.chat(req).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("auto-compress {project}#{id}: {e}");
                            continue;
                        }
                    };
                    let new_body = resp.content.trim().to_string();
                    if new_body.is_empty() || new_body.len() >= body.len() {
                        // No win — skip but still mark so we don't retry.
                        let guard = store.lock().await;
                        let mut meta = guard.get_metadata(id).unwrap_or_default();
                        meta.insert("compressed_at".into(), now.to_string());
                        meta.insert("compressed_skip".into(), "no-shrink".into());
                        let _ = guard.set_metadata(id, &meta);
                        continue;
                    }
                    let guard = store.lock().await;
                    if let Err(e) = guard.compress_in_place(id, &new_body) {
                        tracing::warn!("auto-compress {project}#{id}: set_body: {e}");
                        continue;
                    }
                    let mut meta = guard.get_metadata(id).unwrap_or_default();
                    meta.insert("compressed_at".into(), now.to_string());
                    meta.insert("compressed_model".into(), model.clone());
                    meta.insert("compressed_from_chars".into(), body.len().to_string());
                    meta.insert("compressed_to_chars".into(), new_body.len().to_string());
                    let _ = guard.set_metadata(id, &meta);
                    tracing::info!(
                        project = %project,
                        id,
                        from = body.len(),
                        to = new_body.len(),
                        "auto-compressed"
                    );
                }
            }
        }
    });
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn stats() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "input_saved": 0,
        "output_saved": 0,
        "provider": null,
    }))
}

async fn list_templates() -> Json<Vec<TemplateSummary>> {
    Json(
        rtrt_templates::list_all()
            .into_iter()
            .map(TemplateSummary::from)
            .collect(),
    )
}

async fn get_template(
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<rtrt_templates::Template>, (StatusCode, String)> {
    rtrt_templates::find(&name)
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("template not found: {name}")))
}

#[derive(Debug, Clone, Serialize)]
struct TemplateSummary {
    name: String,
    description: String,
    source: rtrt_templates::TemplateSource,
    category: rtrt_templates::TemplateCategory,
    variables: Vec<rtrt_templates::TemplateVariable>,
}

impl From<rtrt_templates::Template> for TemplateSummary {
    fn from(t: rtrt_templates::Template) -> Self {
        Self {
            name: t.name,
            description: t.description,
            source: t.source,
            category: t.category,
            variables: t.variables,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ScaffoldRequest {
    template: String,
    target: PathBuf,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Serialize)]
struct ScaffoldResponse {
    files_written: usize,
    root: PathBuf,
    post_hooks: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ScaffoldPreviewResponse {
    root: PathBuf,
    files: Vec<ScaffoldPreviewFile>,
    post_hooks: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ScaffoldPreviewFile {
    path: PathBuf,
    bytes: usize,
    executable: bool,
}

async fn scaffold_preview(
    Json(req): Json<ScaffoldRequest>,
) -> std::result::Result<Json<ScaffoldPreviewResponse>, (StatusCode, String)> {
    let tmpl = rtrt_templates::find(&req.template).ok_or((
        StatusCode::NOT_FOUND,
        format!("template not found: {}", req.template),
    ))?;
    let plan = rtrt_templates::render::plan(&tmpl, &req.target, req.variables)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let files = plan
        .files
        .iter()
        .map(|f| ScaffoldPreviewFile {
            path: f.path.clone(),
            bytes: f.content.len(),
            executable: f.executable,
        })
        .collect();
    Ok(Json(ScaffoldPreviewResponse {
        root: plan.root,
        files,
        post_hooks: plan.post_hooks,
    }))
}

async fn scaffold(
    Json(req): Json<ScaffoldRequest>,
) -> std::result::Result<Json<ScaffoldResponse>, (StatusCode, String)> {
    let tmpl = rtrt_templates::find(&req.template).ok_or((
        StatusCode::NOT_FOUND,
        format!("template not found: {}", req.template),
    ))?;
    let plan = rtrt_templates::render::plan(&tmpl, &req.target, req.variables)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    rtrt_templates::render::write(&plan, req.overwrite)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ScaffoldResponse {
        files_written: plan.files.len(),
        root: plan.root,
        post_hooks: plan.post_hooks,
    }))
}

#[derive(Debug, Deserialize)]
struct ChatHttpRequest {
    model: String,
    messages: Vec<ChatHttpMessage>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct ChatHttpMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatHttpResponse {
    provider: String,
    model: String,
    content: String,
    input_tokens: u64,
    output_tokens: u64,
}

async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatHttpRequest>,
) -> std::result::Result<Json<ChatHttpResponse>, (StatusCode, String)> {
    let messages = req
        .messages
        .into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            ChatMessage {
                role,
                content: m.content,
            }
        })
        .collect();
    let chat_req = ChatRequest {
        model: req.model,
        messages,
        max_tokens: req.max_tokens,
        temperature: req.temperature,
    };
    let resp = state
        .gateway
        .chat(chat_req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "chat".into());
    meta.insert("provider".into(), resp.provider.clone());
    meta.insert("model".into(), resp.model.clone());
    meta.insert("input_tokens".into(), resp.usage.input_tokens.to_string());
    meta.insert("output_tokens".into(), resp.usage.output_tokens.to_string());
    state.capture("chat", &resp.content, &meta).await;
    Ok(Json(ChatHttpResponse {
        provider: resp.provider,
        model: resp.model,
        content: resp.content,
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
    }))
}

#[derive(Debug, Serialize)]
struct MetricsResponse {
    summary: rtrt_providers::GatewaySummary,
    by_provider: BTreeMap<String, rtrt_providers::GatewaySummary>,
    recent: Vec<RequestMetric>,
}

fn open_memory_store() -> Option<Arc<Mutex<MemoryStore>>> {
    let path = std::env::var("RTRT_MEMORY_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".rtrt/memory.sqlite"));
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    match MemoryStore::open(&path) {
        Ok(store) => Some(Arc::new(Mutex::new(store))),
        Err(e) => {
            tracing::warn!(?path, "memory store unavailable: {e}");
            None
        }
    }
}

#[derive(Debug, Deserialize)]
struct MemoryRecallRequest {
    project: String,
    query: String,
    #[serde(default = "default_recall_limit")]
    limit: u32,
    #[serde(default)]
    filter: Option<String>,
}

fn default_recall_limit() -> u32 {
    10
}

async fn memory_recall(
    State(state): State<AppState>,
    Json(req): Json<MemoryRecallRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let hits = match req.filter.as_deref() {
        Some(spec) if !spec.is_empty() => {
            let f =
                PayloadFilter::parse(spec).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            guard
                .recall_bm25_with_filter(&req.project, &req.query, req.limit as usize, &f)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        }
        _ => guard
            .recall_bm25(&req.project, &req.query, req.limit as usize)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    };
    Ok(Json(serde_json::json!({ "hits": hits })))
}

async fn memory_projects(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let rows = guard
        .projects()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let projects: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(name, count, latest)| {
            serde_json::json!({ "project": name, "count": count, "latest_ts": latest })
        })
        .collect();
    Ok(Json(serde_json::json!({ "projects": projects })))
}

#[derive(Debug, Deserialize)]
struct MemoryTimelineQuery {
    project: String,
    #[serde(default = "default_timeline_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_timeline_limit() -> usize {
    50
}

async fn memory_timeline(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryTimelineQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let rows = guard
        .recent_paged(&q.project, q.limit, q.offset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total = guard
        .count_by_project(&q.project)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            // `body_full` is the preserved pre-compression original (None
            // when the row was never compressed). `body` is what recall
            // uses (terse when compressed).
            let full = guard.full_body(r.id).ok().flatten();
            let compressed = full.is_some();
            serde_json::json!({
                "id": r.id,
                "kind": r.kind,
                "scope": r.scope,
                "body": r.body,
                "body_full": full,
                "compressed": compressed,
                "created_at": r.created_at,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "items": items,
        "total": total,
        "limit": q.limit,
        "offset": q.offset,
    })))
}

#[derive(Debug, Deserialize)]
struct MemoryGraphQuery {
    project: String,
    #[serde(default = "default_graph_limit")]
    limit: usize,
}

fn default_graph_limit() -> usize {
    200
}

async fn memory_graph(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryGraphQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let records = guard
        .list_by_project(&q.project, q.limit)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let edges = guard
        .project_edges(&q.project)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let nodes: Vec<serde_json::Value> = records
        .into_iter()
        .map(|r| {
            let preview: String = r.body.chars().take(60).collect();
            serde_json::json!({
                "id": r.id,
                "kind": r.kind,
                "scope": r.scope,
                "preview": preview,
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = edges
        .into_iter()
        .map(|(s, d, rel)| serde_json::json!({"src": s, "dst": d, "relation": rel}))
        .collect();
    Ok(Json(serde_json::json!({
        "nodes": nodes,
        "edges": edges,
    })))
}

#[derive(Debug, Deserialize)]
struct MemoryExportQuery {
    project: String,
}

async fn memory_export(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<MemoryExportQuery>,
) -> std::result::Result<axum::response::Response, (StatusCode, String)> {
    use axum::http::header;
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let mut buf = Vec::new();
    guard
        .export_jsonl(&q.project, &mut buf)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let filename = format!("{}.jsonl", q.project.replace(['/', '\\'], "_"));
    let disposition = format!("attachment; filename=\"{filename}\"");
    let resp = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(axum::body::Body::from(buf))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(resp)
}

#[derive(Debug, Deserialize)]
struct MemorySaveRequest {
    project: String,
    #[serde(default = "default_kind")]
    kind: String,
    body: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

fn default_kind() -> String {
    "note".into()
}

async fn bearer_guard(
    expected: Option<Arc<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{HeaderValue, header::AUTHORIZATION};
    let path = req.uri().path().to_string();
    // Always allow the bundled HTML, health probe, and favicon so the UI can
    // bootstrap; the API routes still require the token.
    if matches!(path.as_str(), "/" | "/healthz" | "/favicon.ico") {
        return next.run(req).await;
    }
    let Some(expected) = expected else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.strip_prefix("Bearer "))
        .map(str::to_string);
    let ok = presented
        .as_deref()
        .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));
    if ok {
        return next.run(req).await;
    }
    let mut resp = axum::response::Response::new(axum::body::Body::from(
        "unauthorized: bearer token missing or invalid",
    ));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        "WWW-Authenticate",
        HeaderValue::from_static("Bearer realm=\"rtrt-dashboard\""),
    );
    resp
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Debug, Deserialize)]
struct SetBlockRequest {
    project: String,
    name: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct ListBlocksQuery {
    project: String,
}

async fn list_blocks(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListBlocksQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let blocks = guard
        .list_blocks(&q.project)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "blocks": blocks })))
}

async fn set_block(
    State(state): State<AppState>,
    Json(req): Json<SetBlockRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let id = guard
        .set_block(&req.project, &req.name, &req.body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
struct GetBlockQuery {
    project: String,
}

async fn get_block(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<GetBlockQuery>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let block = guard
        .get_block(&q.project, &name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match block {
        Some(b) => {
            Ok(Json(serde_json::to_value(&b).map_err(|e| {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?))
        }
        None => Err((StatusCode::NOT_FOUND, format!("block not found: {name}"))),
    }
}

async fn memory_save(
    State(state): State<AppState>,
    Json(req): Json<MemorySaveRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;
    let guard = store.lock().await;
    let id = if req.metadata.is_empty() {
        guard
            .save(&req.project, &req.kind, &req.body)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    } else {
        guard
            .save_with_metadata(&req.project, &req.kind, &req.body, &req.metadata)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    Ok(Json(serde_json::json!({ "id": id })))
}

#[derive(Debug, Deserialize)]
struct CompressRequest {
    text: String,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    ml: bool,
    #[serde(default)]
    ratio: Option<f32>,
    #[serde(default)]
    format: Option<String>,
    /// Engine selector: "rules" (default), "ml", or "llm".
    /// When "llm", `model` must be set; the gateway's configured base URL is used.
    #[serde(default)]
    engine: Option<String>,
    /// Model id passed to the gateway when `engine = "llm"`.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyRequest {
    /// Optional command label (e.g. `git status`, `cargo build`).
    #[serde(default)]
    command: Option<String>,
    /// Raw captured output to filter.
    raw: String,
    /// Filter mode: `command` (default — picks `filter_for(command)`),
    /// `errors_only`, or `ultra_compact`.
    #[serde(default)]
    mode: Option<String>,
    /// Context-line count for `errors_only` (default 3).
    #[serde(default = "default_context")]
    context: usize,
}

fn default_context() -> usize {
    3
}

async fn proxy_filter(
    State(state): State<AppState>,
    Json(req): Json<ProxyRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mode = req.mode.as_deref().unwrap_or("command");
    let original = req.raw.chars().count();
    let out = match mode {
        "command" => {
            let cmd = req.command.as_deref().ok_or((
                StatusCode::BAD_REQUEST,
                "command required for command-mode".into(),
            ))?;
            match rtrt_proxy::filter_for(cmd) {
                Some(f) => f.apply(&req.raw),
                None => req.raw.clone(),
            }
        }
        "errors_only" => rtrt_proxy::errors_only(&req.raw, req.context),
        "ultra_compact" => rtrt_proxy::ultra_compact(&req.raw),
        other => return Err((StatusCode::BAD_REQUEST, format!("unknown mode: {other}"))),
    };
    let filtered = out.chars().count();
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "proxy".into());
    meta.insert("mode".into(), mode.to_string());
    if let Some(cmd) = req.command.as_deref() {
        meta.insert("command".into(), cmd.to_string());
    }
    meta.insert(
        "saved_chars".into(),
        original.saturating_sub(filtered).to_string(),
    );
    state.capture("proxy", &out, &meta).await;
    Ok(Json(serde_json::json!({
        "filtered": out,
        "mode": mode,
        "original_len": original,
        "filtered_len": filtered,
        "saved_chars": original.saturating_sub(filtered),
    })))
}

#[derive(Debug, Deserialize)]
struct DiagnoseRequest {
    /// Raw captured output (typically build/test failure).
    raw: String,
    /// Model id to send to the gateway.
    model: String,
    /// errors_only context-line count.
    #[serde(default = "default_context")]
    context: usize,
    /// Optional system prompt override; defaults to the rtrt-cli triage prompt.
    #[serde(default)]
    system: Option<String>,
}

const DIAGNOSE_SYS: &str = "You are a senior engineer triaging a build / test failure. \
Read the captured error output and respond with: (1) one-sentence root cause; \
(2) the smallest concrete fix (file + change). No filler. Cite line numbers when present.";

async fn diagnose(
    State(state): State<AppState>,
    Json(req): Json<DiagnoseRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let filtered = rtrt_proxy::errors_only(&req.raw, req.context);
    let chat_req = ChatRequest {
        model: req.model.clone(),
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: req.system.unwrap_or_else(|| DIAGNOSE_SYS.into()),
            },
            ChatMessage {
                role: Role::User,
                content: filtered.clone(),
            },
        ],
        max_tokens: Some(512),
        temperature: Some(0.1),
    };
    let resp = state
        .gateway
        .chat(chat_req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "diagnose".into());
    meta.insert("provider".into(), resp.provider.clone());
    meta.insert("model".into(), resp.model.clone());
    state.capture("diagnose", &resp.content, &meta).await;
    Ok(Json(serde_json::json!({
        "diagnosis": resp.content,
        "filtered": filtered,
        "provider": resp.provider,
        "model": resp.model,
        "input_tokens": resp.usage.input_tokens,
        "output_tokens": resp.usage.output_tokens,
    })))
}

#[derive(Debug, Deserialize)]
struct RepoMapRequest {
    root: PathBuf,
    #[serde(default = "default_ext")]
    ext: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: u64,
}

fn default_ext() -> String {
    // Empty = auto-detect via `Language::from_filename` (.rs / .py / .ts / .tsx).
    String::new()
}
fn default_max_bytes() -> u64 {
    524_288
}

async fn repo_map(
    Json(req): Json<RepoMapRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    if !req.root.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!("root not found: {}", req.root.display()),
        ));
    }
    let mut entries = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut signature_chars: usize = 0;
    let restrict_ext = req.ext.trim();
    for entry in walk_files(&req.root) {
        let name = entry.to_string_lossy();
        if !restrict_ext.is_empty() && !name.ends_with(restrict_ext) {
            continue;
        }
        let Some(lang) = rtrt_compress::Language::from_filename(&name) else {
            continue;
        };
        let size = std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
        if size > req.max_bytes {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let extractor = rtrt_compress::SignatureExtractor::new(lang);
        let Ok(sig) = extractor.extract(&src) else {
            continue;
        };
        total_bytes += src.len() as u64;
        signature_chars += sig.chars().count();
        let rel = entry
            .strip_prefix(&req.root)
            .unwrap_or(&entry)
            .display()
            .to_string();
        entries.push(serde_json::json!({
            "path": rel,
            "language": format!("{lang:?}"),
            "signatures": sig,
            "original_bytes": src.len(),
            "signature_bytes": sig.len(),
        }));
    }
    Ok(Json(serde_json::json!({
        "files": entries.len(),
        "total_bytes": total_bytes,
        "signature_chars": signature_chars,
        "entries": entries,
    })))
}

fn walk_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    let name = entry.file_name();
                    let nstr = name.to_string_lossy();
                    if nstr == "target" || nstr.starts_with('.') {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() {
                    out.push(path);
                }
            }
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct SetupRequest {
    agent: String,
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    binary: Option<String>,
}

async fn setup_snippet(
    Json(req): Json<SetupRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    let binary = req.binary.clone().unwrap_or_else(|| "rtrt-mcp".to_string());
    let memory = req
        .memory
        .clone()
        .unwrap_or_else(|| ".rtrt/memory.sqlite".to_string());
    let (target_path, snippet) = match req.agent.as_str() {
        "claude-code" => (
            "~/.claude/mcp.json".to_string(),
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "rtrt": {
                        "command": binary,
                        "args": ["--memory", memory]
                    }
                }
            }))
            .unwrap(),
        ),
        "cursor" => (
            "~/.cursor/mcp.json".to_string(),
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "rtrt": {
                        "command": binary,
                        "args": ["--memory", memory]
                    }
                }
            }))
            .unwrap(),
        ),
        "codex" => (
            "~/.codex/config.toml".to_string(),
            format!(
                "[mcp.rtrt]\ncommand = \"{}\"\nargs = [\"--memory\", \"{}\"]\n",
                binary, memory
            ),
        ),
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown agent: {other} (try claude-code / cursor / codex)"),
            ));
        }
    };
    Ok(Json(serde_json::json!({
        "agent": req.agent,
        "target_path": target_path,
        "snippet": snippet,
    })))
}

const LLM_COMPRESS_SYS: &str = "You are a lossless-meaning compressor. Rewrite the user message \
in the shortest form that preserves every fact, decision, file path, identifier, command, and \
number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary, no \
preamble, no quotes — emit only the compressed text.";

async fn compress(
    State(state): State<AppState>,
    Json(req): Json<CompressRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    use rtrt_core::CompressionLevel;
    let original = req.text.chars().count();

    // Resolve the effective engine: explicit `engine` field wins; fall back to
    // the legacy `ml: true` boolean for backward compat.
    let engine = req
        .engine
        .as_deref()
        .unwrap_or(if req.ml { "ml" } else { "rules" });

    match engine {
        "llm" => {
            let model = req.model.clone().ok_or((
                StatusCode::BAD_REQUEST,
                "llm engine requires a `model` field".into(),
            ))?;
            // Build a request-scoped gateway that honours Config::auto_compress.base_url
            // when neither RTRT_PROVIDER_BASE_URL nor RTRT_OPENAI_COMPAT_URL is set —
            // the same resolution order used by run_hook_compress in rtrt-cli. state.gateway
            // was constructed at startup before config base_url was available, so we rebuild
            // here. When the env vars are absent we temporarily set RTRT_PROVIDER_BASE_URL
            // from config so Gateway::from_env registers the openai-compat provider.
            let llm_gateway = {
                let env_has_url = std::env::var_os("RTRT_PROVIDER_BASE_URL").is_some()
                    || std::env::var_os("RTRT_OPENAI_COMPAT_URL").is_some();
                if env_has_url {
                    rtrt_providers::Gateway::from_env()
                } else {
                    let cfg_url = rtrt_core::Config::load()
                        .ok()
                        .and_then(|c| c.auto_compress.base_url);
                    if let Some(url) = cfg_url {
                        // SAFETY: no await between set_var and remove_var; the var was
                        // absent before this block so concurrent handlers that reach this
                        // branch independently each set-and-remove their own value.
                        unsafe { std::env::set_var("RTRT_PROVIDER_BASE_URL", &url) };
                        let gw = rtrt_providers::Gateway::from_env();
                        unsafe { std::env::remove_var("RTRT_PROVIDER_BASE_URL") };
                        gw
                    } else {
                        rtrt_providers::Gateway::from_env()
                    }
                }
            };
            let chat_req = ChatRequest {
                model: model.clone(),
                messages: vec![
                    ChatMessage {
                        role: Role::System,
                        content: LLM_COMPRESS_SYS.to_string(),
                    },
                    ChatMessage {
                        role: Role::User,
                        content: req.text.clone(),
                    },
                ],
                max_tokens: Some(
                    // Cap at 4× the original char count but at least 128 tokens.
                    (original as u32).saturating_mul(4).clamp(128, 4096),
                ),
                temperature: Some(0.0),
            };
            let resp = llm_gateway
                .chat(chat_req)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            let out = resp.content.trim().to_string();
            let compressed = out.chars().count();
            let mut meta = BTreeMap::new();
            meta.insert("source".into(), "compress".into());
            meta.insert("mode".into(), "llm".into());
            meta.insert("model".into(), model.clone());
            meta.insert(
                "saved_chars".into(),
                original.saturating_sub(compressed).to_string(),
            );
            state.capture("compress", &out, &meta).await;
            Ok(Json(serde_json::json!({
                "compressed": out,
                "mode": "llm",
                "model": model,
                "original_len": original,
                "compressed_len": compressed,
                "saved": original.saturating_sub(compressed),
                "saved_chars": original.saturating_sub(compressed),
            })))
        }

        "ml" => {
            let target = rtrt_compress::CompressionTarget::new(req.ratio.unwrap_or(0.5))
                .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
            let c = rtrt_compress::MlCompressor::heuristic();
            let scorer = c.scorer_name().to_string();
            let out = c.compress(&req.text, target);
            let compressed = out.chars().count();
            let mut meta = BTreeMap::new();
            meta.insert("source".into(), "compress".into());
            meta.insert("mode".into(), "ml".into());
            meta.insert("scorer".into(), scorer.clone());
            meta.insert(
                "saved_chars".into(),
                original.saturating_sub(compressed).to_string(),
            );
            state.capture("compress", &out, &meta).await;
            Ok(Json(serde_json::json!({
                "compressed": out,
                "mode": "ml",
                "scorer": scorer,
                "original_len": original,
                "compressed_len": compressed,
                "saved": original.saturating_sub(compressed),
                "saved_chars": original.saturating_sub(compressed),
            })))
        }

        // "rules" or any unrecognised value — run the rule-based compressor.
        _ => {
            let level = match req.level.as_deref().unwrap_or("full") {
                "lite" => CompressionLevel::Lite,
                "full" => CompressionLevel::Full,
                "ultra" => CompressionLevel::Ultra,
                "extreme" => CompressionLevel::Extreme,
                other => {
                    return Err((StatusCode::BAD_REQUEST, format!("unknown level: {other}")));
                }
            };
            let compressor = rtrt_compress::Compressor::new(level);
            let format = req
                .format
                .as_deref()
                .and_then(rtrt_compress::OutputFormat::parse)
                .unwrap_or(rtrt_compress::OutputFormat::Plain);
            let out = compressor.compress_to(&req.text, format);
            let compressed = out.chars().count();
            let mut meta = BTreeMap::new();
            meta.insert("source".into(), "compress".into());
            meta.insert("mode".into(), "rules".into());
            meta.insert(
                "saved_chars".into(),
                original.saturating_sub(compressed).to_string(),
            );
            state.capture("compress", &out, &meta).await;
            Ok(Json(serde_json::json!({
                "compressed": out,
                "mode": "rules",
                "original_len": original,
                "compressed_len": compressed,
                "saved": original.saturating_sub(compressed),
                "saved_chars": original.saturating_sub(compressed),
            })))
        }
    }
}

fn open_prompt_registry() -> Option<Arc<PromptRegistry>> {
    let root = match std::env::var("RTRT_PROMPTS_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => rtrt_templates::prompts::default_dir()?,
    };
    match PromptRegistry::open(&root) {
        Ok(reg) => Some(Arc::new(reg)),
        Err(e) => {
            tracing::warn!(?root, "prompts registry unavailable: {e}");
            None
        }
    }
}

#[derive(Debug, Serialize)]
struct PromptSummary {
    name: String,
    versions: Vec<u32>,
    latest: u32,
}

fn require_prompts(state: &AppState) -> std::result::Result<&PromptRegistry, (StatusCode, String)> {
    state
        .prompts
        .as_deref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "prompts disabled".into()))
}

async fn list_prompts(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<PromptSummary>>, (StatusCode, String)> {
    let reg = require_prompts(&state)?;
    let names = reg
        .list_names()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let versions = reg
            .list_versions(&name)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let latest = versions.iter().copied().max().unwrap_or(0);
        out.push(PromptSummary {
            name,
            versions,
            latest,
        });
    }
    Ok(Json(out))
}

async fn list_prompt_versions(
    State(state): State<AppState>,
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<PromptSummary>, (StatusCode, String)> {
    let reg = require_prompts(&state)?;
    let versions = reg
        .list_versions(&name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if versions.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("prompt not found: {name}")));
    }
    let latest = versions.iter().copied().max().unwrap_or(0);
    Ok(Json(PromptSummary {
        name,
        versions,
        latest,
    }))
}

async fn get_prompt(
    State(state): State<AppState>,
    AxPath((name, version)): AxPath<(String, u32)>,
) -> std::result::Result<Json<Prompt>, (StatusCode, String)> {
    let reg = require_prompts(&state)?;
    reg.get(&name, version)
        .map(Json)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))
}

#[derive(Debug, Serialize)]
struct BudgetResponse {
    cap_usd: Option<f64>,
    spent_usd: f64,
    remaining_usd: Option<f64>,
    cache_len: Option<usize>,
}

async fn sse_stream(
    State(state): State<AppState>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<
        Item = std::result::Result<axum::response::sse::Event, std::convert::Infallible>,
    >,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::StreamExt;
    let rx = state.events.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(line) => Some(Ok::<_, std::convert::Infallible>(
                Event::default().data(line),
            )),
            Err(_lag) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn tokens_summary(State(state): State<AppState>) -> Json<serde_json::Value> {
    let buf = state.gateway.metrics();
    let guard = buf.lock().unwrap_or_else(|p| p.into_inner());
    use rtrt_providers::MetricsView;
    let view = MetricsView::new(&guard);
    let summary = view.summary();
    let by_provider = view.by_provider();
    let recent = view.recent(usize::MAX);
    // Bucket by hour (Unix epoch / 3600). Stable across timezones.
    let mut hourly: std::collections::BTreeMap<u64, (u64, u64, u64)> = Default::default();
    let mut daily: std::collections::BTreeMap<u64, (u64, u64, u64)> = Default::default();
    for m in &recent {
        let h: u64 = m.started_at / 3600;
        let d: u64 = m.started_at / 86400;
        let e = hourly.entry(h).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += m.usage.input_tokens;
        e.2 += m.usage.output_tokens;
        let e = daily.entry(d).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += m.usage.input_tokens;
        e.2 += m.usage.output_tokens;
    }
    let hourly: Vec<_> = hourly
        .into_iter()
        .map(|(h, (c, i, o))| {
            serde_json::json!({"hour_ts": h*3600, "calls": c, "input_tokens": i, "output_tokens": o})
        })
        .collect();
    let daily: Vec<_> = daily
        .into_iter()
        .map(|(d, (c, i, o))| {
            serde_json::json!({"day_ts": d*86400, "calls": c, "input_tokens": i, "output_tokens": o})
        })
        .collect();
    Json(serde_json::json!({
        "summary": summary,
        "by_provider": by_provider,
        "hourly": hourly,
        "daily": daily,
    }))
}

async fn budget(State(state): State<AppState>) -> Json<BudgetResponse> {
    let cap = state.gateway.budget_cap_usd();
    let spent = state.gateway.budget_spent_usd();
    let remaining = cap.map(|c| (c - spent).max(0.0));
    Json(BudgetResponse {
        cap_usd: cap,
        spent_usd: spent,
        remaining_usd: remaining,
        cache_len: state.gateway.cache_len(),
    })
}

async fn metrics(State(state): State<AppState>) -> Json<MetricsResponse> {
    let metrics = state.gateway.metrics();
    let guard = metrics.lock().unwrap_or_else(|p| p.into_inner());
    let view = MetricsView::new(&guard);
    let by_provider = view
        .by_provider()
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    Json(MetricsResponse {
        summary: view.summary(),
        by_provider,
        recent: view.recent(50),
    })
}

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ConfigResponse {
    capture: rtrt_core::config::CaptureConfig,
    auto_compress: rtrt_core::config::AutoCompressConfig,
    path: String,
}

async fn get_config() -> std::result::Result<Json<ConfigResponse>, (StatusCode, String)> {
    let cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let path = rtrt_core::Config::default_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Json(ConfigResponse {
        capture: cfg.capture,
        auto_compress: cfg.auto_compress,
        path,
    }))
}

// ---------------------------------------------------------------------------
// POST /api/config
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ConfigWriteRequest {
    capture: rtrt_core::config::CaptureConfig,
    auto_compress: rtrt_core::config::AutoCompressConfig,
}

#[derive(Debug, Serialize)]
struct ConfigWriteResponse {
    ok: bool,
    path: String,
}

async fn post_config(
    Json(req): Json<ConfigWriteRequest>,
) -> std::result::Result<Json<ConfigWriteResponse>, (StatusCode, String)> {
    // Build an updated Config preserving any non-exposed fields from disk.
    let mut cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    cfg.capture = req.capture;
    cfg.auto_compress = req.auto_compress;

    let path = rtrt_core::Config::default_path().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "cannot determine config path".into(),
    ))?;

    // Create parent directory if needed.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("create dir {}: {e}", parent.display()),
            )
        })?;
    }

    // Back up the existing file before overwriting.
    if path.exists() {
        let bak = path.with_extension("toml.bak");
        std::fs::copy(&path, &bak).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("backup {}: {e}", path.display()),
            )
        })?;
    }

    let toml_str = toml::to_string_pretty(&cfg)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&path, toml_str).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;

    Ok(Json(ConfigWriteResponse {
        ok: true,
        path: path.to_string_lossy().into_owned(),
    }))
}

// ---------------------------------------------------------------------------
// GET /api/models
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ModelEntry {
    id: String,
    source: &'static str,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    models: Vec<ModelEntry>,
}

async fn get_models() -> Json<ModelsResponse> {
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    // Derive the Ollama host root: strip a trailing `/v1` (OpenAI-compat
    // path prefix) and any trailing slash so `/api/tags` lands at the right
    // place regardless of how base_url was configured.
    let ollama_host = cfg
        .auto_compress
        .base_url
        .as_deref()
        .unwrap_or("http://127.0.0.1:11434")
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/')
        .to_string();

    let mut models: Vec<ModelEntry> = Vec::new();

    // Attempt to list Ollama models; any failure is silently ignored.
    let ollama_url = format!("{ollama_host}/api/tags");
    if let Ok(resp) = reqwest::Client::new()
        .get(&ollama_url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        if let Ok(body) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = body.get("models").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(name) = m.get("name").and_then(|v| v.as_str()) {
                        models.push(ModelEntry {
                            id: name.to_string(),
                            source: "ollama",
                        });
                    }
                }
            }
        }
    }

    // Always append the cloud defaults.
    models.push(ModelEntry {
        id: "claude-haiku-4-5".to_string(),
        source: "cloud",
    });
    models.push(ModelEntry {
        id: "gpt-5.4-mini".to_string(),
        source: "cloud",
    });

    Json(ModelsResponse { models })
}

// ---------------------------------------------------------------------------
// POST /api/memory/compress
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryCompressRequest {
    project: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct MemoryCompressResponse {
    compressed: usize,
    skipped: usize,
}

async fn memory_compress(
    State(state): State<AppState>,
    Json(req): Json<MemoryCompressRequest>,
) -> std::result::Result<Json<MemoryCompressResponse>, (StatusCode, String)> {
    let store = state
        .memory
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "memory disabled".into()))?;

    let cfg = rtrt_core::Config::load()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let ac = &cfg.auto_compress;

    let age_sec: i64 = ac.age_sec;
    let min_chars: usize = ac.min_chars;
    let batch: usize = ac.batch;
    let max_tokens: u32 = ac.max_tokens;
    let model = req.model.clone().unwrap_or_else(|| ac.model.clone());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now - age_sec;

    let candidates = {
        let guard = store.lock().await;
        guard
            .compress_candidates(&req.project, cutoff, min_chars, batch)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    let mut compressed_count = 0usize;
    let mut skipped_count = 0usize;

    for (id, body) in candidates {
        let chat_req = ChatRequest {
            model: model.clone(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are a lossless-meaning compressor. Rewrite the user message in the shortest form that preserves every fact, decision, file path, identifier, command, and number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary, no preamble, no quotes — emit only the compressed text.".to_string(),
                },
                ChatMessage {
                    role: Role::User,
                    content: body.clone(),
                },
            ],
            max_tokens: Some(max_tokens),
            temperature: Some(0.0),
        };
        let resp = match state.gateway.chat(chat_req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("memory_compress {project}#{id}: {e}", project = req.project);
                skipped_count += 1;
                continue;
            }
        };
        let new_body = resp.content.trim().to_string();
        if new_body.is_empty() || new_body.len() >= body.len() {
            // No compression win; mark so the next sweep skips this row.
            let guard = store.lock().await;
            let mut meta = guard.get_metadata(id).unwrap_or_default();
            meta.insert("compressed_at".into(), now.to_string());
            meta.insert("compressed_skip".into(), "no-shrink".into());
            let _ = guard.set_metadata(id, &meta);
            skipped_count += 1;
            continue;
        }
        let guard = store.lock().await;
        if let Err(e) = guard.compress_in_place(id, &new_body) {
            tracing::warn!(
                "memory_compress {project}#{id}: compress_in_place: {e}",
                project = req.project
            );
            skipped_count += 1;
            continue;
        }
        let mut meta = guard.get_metadata(id).unwrap_or_default();
        meta.insert("compressed_at".into(), now.to_string());
        meta.insert("compressed_model".into(), model.clone());
        meta.insert("compressed_from_chars".into(), body.len().to_string());
        meta.insert("compressed_to_chars".into(), new_body.len().to_string());
        let _ = guard.set_metadata(id, &meta);
        compressed_count += 1;
        tracing::info!(
            project = %req.project,
            id,
            from = body.len(),
            to = new_body.len(),
            "manual compress sweep"
        );
    }

    Ok(Json(MemoryCompressResponse {
        compressed: compressed_count,
        skipped: skipped_count,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/memory/stats?project=X
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryStatsQuery {
    project: String,
}

#[derive(Debug, Serialize)]
struct KindCount {
    kind: String,
    count: i64,
}

#[derive(Debug, Serialize)]
struct DayCount {
    day: String,
    count: i64,
}

#[derive(Debug, Serialize)]
struct MemoryStatsResponse {
    total: i64,
    by_kind: Vec<KindCount>,
    compressed_count: i64,
    saved_chars: i64,
    by_day: Vec<DayCount>,
}

async fn memory_stats(
    axum::extract::Query(q): axum::extract::Query<MemoryStatsQuery>,
) -> std::result::Result<Json<MemoryStatsResponse>, (StatusCode, String)> {
    // Open a direct rusqlite connection to the same path used by open_memory_store().
    let path = std::env::var("RTRT_MEMORY_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".rtrt/memory.sqlite"));

    let conn = rusqlite::Connection::open(&path).map_err(|e| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("memory store: {e}"),
        )
    })?;

    // Total row count for the project.
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE project = ?1",
            rusqlite::params![q.project],
            |row| row.get(0),
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Per-kind breakdown.
    let by_kind = {
        let mut stmt = conn
            .prepare(
                "SELECT kind, COUNT(*) AS cnt FROM memories WHERE project = ?1 GROUP BY kind ORDER BY cnt DESC",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![q.project], |row| {
                Ok(KindCount {
                    kind: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    // compressed_count = rows whose metadata JSON contains "compressed_at".
    // saved_chars = sum of (compressed_from_chars - compressed_to_chars) parsed from metadata.
    // The metadata column stores a JSON object; we query all rows with metadata and filter in Rust
    // to stay compatible with any SQLite version (no JSON1 dependency assumed).
    let (compressed_count, saved_chars) = {
        let mut stmt = conn
            .prepare("SELECT metadata FROM memories WHERE project = ?1 AND metadata IS NOT NULL")
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![q.project], |row| row.get::<_, String>(0))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut count: i64 = 0;
        let mut saved: i64 = 0;
        for meta_str in rows.flatten() {
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&meta_str) {
                if obj.get("compressed_at").is_some() {
                    count += 1;
                    let from: i64 = obj
                        .get("compressed_from_chars")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let to: i64 = obj
                        .get("compressed_to_chars")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    saved += (from - to).max(0);
                }
            }
        }
        (count, saved)
    };

    // by_day: group created_at (Unix timestamp integer) by calendar date (YYYY-MM-DD).
    let by_day = {
        let mut stmt = conn
            .prepare(
                "SELECT date(datetime(created_at, 'unixepoch')) AS day, COUNT(*) AS cnt \
                 FROM memories WHERE project = ?1 GROUP BY day ORDER BY day",
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![q.project], |row| {
                Ok(DayCount {
                    day: row.get(0)?,
                    count: row.get(1)?,
                })
            })
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };

    Ok(Json(MemoryStatsResponse {
        total,
        by_kind,
        compressed_count,
        saved_chars,
        by_day,
    }))
}

const INDEX_HTML: &str = include_str!("../ui/index.html");
