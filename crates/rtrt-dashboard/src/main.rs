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
//! - `/api/memory/save`   — `POST` insert a memory row with optional metadata.
//! - `/api/memory/blocks` — `GET` list / `POST` set Letta-style memory blocks.
//! - `/api/memory/blocks/{name}` — `GET` a single block (project as query param).
//! - `/api/compress`      — `POST` run the rule or ML compressor against arbitrary text.
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

#[derive(Clone)]
struct AppState {
    gateway: Arc<Gateway>,
    prompts: Option<Arc<PromptRegistry>>,
    memory: Option<Arc<Mutex<MemoryStore>>>,
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
    let gateway = Arc::new(Gateway::from_env());
    let prompts = open_prompt_registry();
    let memory = open_memory_store();
    let state = AppState {
        gateway,
        prompts,
        memory,
    };

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
        .route("/api/memory/recall", post(memory_recall))
        .route("/api/memory/export", get(memory_export))
        .route("/api/memory/save", post(memory_save))
        .route("/api/memory/blocks", get(list_blocks).post(set_block))
        .route("/api/memory/blocks/{name}", get(get_block))
        .route("/api/compress", post(compress))
        .route("/api/proxy", post(proxy_filter))
        .route("/api/diagnose", post(diagnose))
        .route("/api/repo-map", post(repo_map))
        .route("/api/setup", post(setup_snippet))
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

async fn compress(
    Json(req): Json<CompressRequest>,
) -> std::result::Result<Json<serde_json::Value>, (StatusCode, String)> {
    use rtrt_core::CompressionLevel;
    let original = req.text.chars().count();
    let (out, mode, scorer) = if req.ml {
        let target = rtrt_compress::CompressionTarget::new(req.ratio.unwrap_or(0.5))
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let c = rtrt_compress::MlCompressor::heuristic();
        let scorer = c.scorer_name().to_string();
        (c.compress(&req.text, target), "ml", Some(scorer))
    } else {
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
        (compressor.compress_to(&req.text, format), "rules", None)
    };
    let compressed = out.chars().count();
    Ok(Json(serde_json::json!({
        "compressed": out,
        "mode": mode,
        "scorer": scorer,
        "original_len": original,
        "compressed_len": compressed,
        "saved_chars": original.saturating_sub(compressed),
    })))
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

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="ko">
<head>
<meta charset="utf-8">
<title>RTRT</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
  :root {
    --bg: #fafbfc; --surface: #ffffff; --fg: #0f172a; --muted: #64748b;
    --border: #e5e7eb; --accent: #2962FF; --accent-soft: #dbe6ff;
    --ok: #16a34a; --warn: #d97706; --err: #dc2626; --grad: linear-gradient(135deg,#2962FF 0%,#7c3aed 100%);
    --shadow: 0 1px 2px rgba(15,23,42,.06), 0 4px 12px rgba(15,23,42,.06);
  }
  :root[data-theme="dark"] {
    --bg: #0b0e14; --surface: #14181f; --fg: #e6e6e6; --muted: #94a3b8;
    --border: #232a35; --accent: #6aa3ff; --accent-soft: #1a2540;
    --ok: #4ade80; --warn: #fbbf24; --err: #f87171; --grad: linear-gradient(135deg,#6aa3ff 0%,#a78bfa 100%);
    --shadow: 0 1px 2px rgba(0,0,0,.4), 0 4px 12px rgba(0,0,0,.35);
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; padding: 0; background: var(--bg); color: var(--fg); font: 14px/1.5 system-ui, -apple-system, sans-serif; min-height: 100vh; }
  a { color: var(--accent); text-decoration: none; cursor: pointer; }

  /* Top status bar */
  .topbar { position: sticky; top: 0; z-index: 10; display: flex; align-items: center; gap: 1rem; padding: 0.6rem 1.25rem; background: var(--surface); border-bottom: 1px solid var(--border); }
  .topbar .brand { font-weight: 700; font-size: 1.05rem; display: flex; align-items: center; gap: 0.5rem; }
  .topbar .brand .logo { width: 28px; height: 28px; border-radius: 8px; background: var(--grad); display: inline-flex; align-items: center; justify-content: center; color: white; font-weight: 800; font-size: 0.78rem; }
  .topbar .pill { display: inline-flex; align-items: center; gap: 0.4rem; padding: 0.25rem 0.6rem; border-radius: 999px; background: var(--bg); border: 1px solid var(--border); font-size: 0.82em; color: var(--muted); }
  .topbar .pill .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--muted); }
  .topbar .pill.ok .dot { background: var(--ok); box-shadow: 0 0 0 3px rgba(22,163,74,.18); }
  .topbar .pill.warn .dot { background: var(--warn); }
  .topbar .spacer { flex: 1; }
  .topbar #theme-toggle { cursor: pointer; background: transparent; border: 1px solid var(--border); color: var(--fg); padding: 0.3rem 0.6rem; border-radius: 6px; font-size: 0.9em; }

  /* Layout: sidebar + main */
  .layout { display: grid; grid-template-columns: 220px 1fr; min-height: calc(100vh - 53px - 32px); }
  aside { background: var(--surface); border-right: 1px solid var(--border); padding: 1rem 0.5rem; }
  aside .group { color: var(--muted); font-size: 0.72em; letter-spacing: 0.08em; text-transform: uppercase; padding: 0.6rem 0.75rem 0.25rem; }
  aside a.nav { display: flex; align-items: center; gap: 0.6rem; padding: 0.55rem 0.75rem; border-radius: 8px; color: var(--fg); margin: 0.1rem 0; }
  aside a.nav:hover { background: var(--bg); }
  aside a.nav.active { background: var(--accent-soft); color: var(--accent); font-weight: 600; }
  aside a.nav .icon { width: 18px; text-align: center; }
  main { padding: 1.5rem 2rem 4rem; max-width: 1100px; }

  /* Section heading */
  .section-head { margin-bottom: 1.25rem; }
  .section-head h1 { margin: 0 0 0.25rem; font-size: 1.4rem; }
  .section-head .lede { color: var(--muted); }

  /* Subtab bar */
  .subtabs { display: flex; gap: 0.4rem; margin-bottom: 1.5rem; border-bottom: 1px solid var(--border); flex-wrap: wrap; }
  .subtabs a { padding: 0.5rem 0.9rem; border-radius: 8px 8px 0 0; color: var(--muted); border-bottom: 2px solid transparent; }
  .subtabs a.active { color: var(--accent); border-bottom-color: var(--accent); }

  /* Cards */
  .card { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 1.25rem; box-shadow: var(--shadow); margin-bottom: 1rem; }
  .card .head { display: flex; align-items: baseline; justify-content: space-between; margin-bottom: 0.75rem; }
  .card h2 { margin: 0; font-size: 1.05rem; }
  .card .hint { color: var(--muted); font-size: 0.9em; }

  /* KPIs */
  .kpis { display: grid; grid-template-columns: repeat(auto-fit, minmax(140px, 1fr)); gap: 0.75rem; }
  .kpi { background: var(--bg); border: 1px solid var(--border); border-radius: 10px; padding: 0.85rem 1rem; transition: transform .15s ease; }
  .kpi:hover { transform: translateY(-1px); }
  .kpi .label { color: var(--muted); font-size: 0.78em; letter-spacing: 0.02em; }
  .kpi .value { font-size: 1.55rem; font-weight: 700; font-variant-numeric: tabular-nums; }
  .kpi .sub { color: var(--muted); font-size: 0.78em; margin-top: 0.1rem; }
  .kpi.accent .value { background: var(--grad); -webkit-background-clip: text; background-clip: text; color: transparent; }

  /* Forms */
  .row { display: grid; grid-template-columns: 1fr 1fr; gap: 0.5rem; }
  form { display: grid; gap: 0.6rem; }
  input, textarea, select, button { background: var(--bg); color: var(--fg); border: 1px solid var(--border); padding: 0.55rem 0.75rem; border-radius: 8px; font: inherit; }
  input:focus, textarea:focus, select:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px rgba(41,98,255,.15); }
  button { cursor: pointer; }
  button.primary { background: var(--accent); color: white; border-color: var(--accent); font-weight: 600; }
  button.primary:hover { filter: brightness(1.06); }
  button.ghost { background: transparent; }
  button:disabled { opacity: 0.6; cursor: progress; }

  /* Tables */
  table { width: 100%; border-collapse: collapse; }
  th { color: var(--muted); font-weight: 500; font-size: 0.82em; letter-spacing: 0.02em; text-transform: uppercase; }
  th, td { padding: 0.55rem 0.5rem; border-bottom: 1px solid var(--border); text-align: left; vertical-align: top; }
  td code { background: var(--bg); padding: 0 0.3rem; border-radius: 4px; font-family: ui-monospace, monospace; font-size: 0.88em; }
  .empty { color: var(--muted); padding: 1rem 0; text-align: center; font-size: 0.9em; }

  /* Output panels */
  pre { background: var(--bg); border: 1px solid var(--border); padding: 0.85rem; border-radius: 8px; white-space: pre-wrap; overflow-x: auto; font-family: ui-monospace, monospace; font-size: 0.85em; }
  .out-meta { color: var(--muted); font-size: 0.85em; margin: 0.5rem 0; }

  /* Sparklines */
  svg.spark { width: 100%; height: 64px; }

  /* Status pills */
  .badge { display: inline-flex; align-items: center; gap: 0.3rem; padding: 0.15rem 0.5rem; border-radius: 999px; font-size: 0.78em; background: var(--bg); border: 1px solid var(--border); }
  .badge.ok { color: var(--ok); border-color: var(--ok); }
  .badge.err { color: var(--err); border-color: var(--err); }
  .badge.warn { color: var(--warn); border-color: var(--warn); }

  /* Live activity strip */
  .activity { position: sticky; bottom: 0; background: var(--surface); border-top: 1px solid var(--border); padding: 0.35rem 1.25rem; font-size: 0.82em; color: var(--muted); display: flex; align-items: center; gap: 0.75rem; max-height: 32px; overflow: hidden; }
  .activity .dot-pulse { width: 6px; height: 6px; border-radius: 50%; background: var(--ok); animation: pulse 1.6s ease-in-out infinite; }
  @keyframes pulse { 0%, 100% { opacity: 0.4; } 50% { opacity: 1; } }
  .activity .feed { flex: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; font-variant-numeric: tabular-nums; }

  /* Count-up animation */
  .countup { display: inline-block; transition: opacity .2s; }

  /* Mobile */
  @media (max-width: 720px) {
    .layout { grid-template-columns: 1fr; }
    aside { display: none; }
    main { padding: 1rem; }
    .row { grid-template-columns: 1fr; }
  }
</style>
</head>
<body>
<div class="topbar">
  <div class="brand"><span class="logo">RT</span> RTRT</div>
  <span id="pill-gateway" class="pill"><span class="dot"></span>게이트웨이</span>
  <span id="pill-memory" class="pill"><span class="dot"></span>메모리</span>
  <span id="pill-cache" class="pill"><span class="dot"></span>캐시</span>
  <span class="spacer"></span>
  <button id="theme-toggle" title="다크/라이트">🌗</button>
</div>

<div class="layout">
  <aside>
    <div class="group">메인</div>
    <a class="nav active" data-page="overview"><span class="icon">🏠</span>개요</a>
    <a class="nav" data-page="memory"><span class="icon">🧠</span>메모리</a>
    <a class="nav" data-page="tools"><span class="icon">🛠</span>도구</a>
    <a class="nav" data-page="settings"><span class="icon">⚙️</span>설정</a>
  </aside>

  <main>
    <!-- 개요 -->
    <section id="page-overview" class="page">
      <div class="section-head">
        <h1>오늘의 절감</h1>
        <div class="lede">RTRT가 줄여준 토큰과 비용을 한눈에.</div>
      </div>
      <div class="card">
        <div class="kpis">
          <div class="kpi accent"><div class="label">절감 토큰</div><div class="value countup" id="kpi-saved" data-target="0">0</div><div class="sub">입력+출력 합산</div></div>
          <div class="kpi"><div class="label">호출 수</div><div class="value countup" id="kpi-calls" data-target="0">0</div><div class="sub">게이트웨이 누적</div></div>
          <div class="kpi"><div class="label">평균 지연</div><div class="value" id="kpi-latency">— ms</div><div class="sub">최근 50건</div></div>
          <div class="kpi"><div class="label">사용 비용</div><div class="value" id="kpi-spent">—</div><div class="sub" id="kpi-spent-sub">한도 미설정</div></div>
        </div>
      </div>
      <div class="card">
        <div class="head"><h2>응답 추이</h2><span class="hint">최근 요청 50건</span></div>
        <div class="row">
          <div><div class="hint">지연 (ms)</div><svg id="chart-latency" class="spark" viewBox="0 0 400 64" preserveAspectRatio="none"></svg></div>
          <div><div class="hint">토큰 (in + out)</div><svg id="chart-tokens" class="spark" viewBox="0 0 400 64" preserveAspectRatio="none"></svg></div>
        </div>
      </div>
      <div class="card">
        <div class="head"><h2>최근 호출</h2><a id="overview-refresh">새로고침</a></div>
        <table id="recent-tbl"><thead><tr><th>시각</th><th>프로바이더</th><th>모델</th><th>in</th><th>out</th><th>지연</th><th>상태</th></tr></thead><tbody></tbody></table>
      </div>
    </section>

    <!-- 메모리 -->
    <section id="page-memory" class="page" hidden>
      <div class="section-head">
        <h1>메모리</h1>
        <div class="lede">프로젝트마다 따로 쌓이는 SQLite 저장소. 검색은 BM25, 옵션으로 필터.</div>
      </div>
      <div class="card">
        <div class="head"><h2>검색</h2><span class="hint">필터 예: <code>source=claude,topic~^auth</code></span></div>
        <form id="recall-form">
          <div class="row">
            <input id="recall-project" placeholder="project (예: rtrt)" required>
            <input id="recall-limit" type="number" min="1" max="50" value="10">
          </div>
          <input id="recall-query" placeholder="검색어" required>
          <input id="recall-filter" placeholder="payload 필터 (선택)">
          <button class="primary" type="submit">검색</button>
        </form>
        <table id="recall-tbl" style="margin-top:0.75rem;"><thead><tr><th>id</th><th>kind</th><th>scope</th><th>내용</th></tr></thead><tbody><tr><td colspan="4" class="empty">검색하면 결과가 여기에 나옵니다.</td></tr></tbody></table>
      </div>
      <div class="card">
        <div class="head"><h2>새 메모리</h2><span class="hint">metadata는 JSON 객체로</span></div>
        <form id="save-form">
          <div class="row">
            <input id="save-project" placeholder="project" required>
            <input id="save-kind" placeholder="kind (기본 note)" value="note">
          </div>
          <textarea id="save-body" rows="3" placeholder="내용" required></textarea>
          <input id="save-metadata" placeholder='{"source":"claude","topic":"auth"}'>
          <button class="primary" type="submit">저장</button>
        </form>
        <div id="save-result" class="out-meta"></div>
      </div>
      <div class="card">
        <div class="head"><h2>블록</h2><span class="hint">persona / human / context 같은 영속 슬롯</span></div>
        <form id="blocks-list-form" style="grid-template-columns:1fr auto;">
          <input id="blocks-project" placeholder="project" required>
          <button type="submit">목록</button>
        </form>
        <table id="blocks-tbl" style="margin-top:0.5rem;"><thead><tr><th>이름</th><th>내용</th></tr></thead><tbody><tr><td colspan="2" class="empty">project를 입력하고 목록을 눌러보세요.</td></tr></tbody></table>
        <form id="blocks-set-form" style="margin-top:0.75rem;">
          <div class="row">
            <input id="block-set-name" placeholder="이름 (persona / human / context)" required>
            <input id="block-set-project" placeholder="project" required>
          </div>
          <textarea id="block-set-body" rows="2" placeholder="블록 내용" required></textarea>
          <button class="primary" type="submit">덮어쓰기</button>
        </form>
        <div id="block-set-result" class="out-meta"></div>
      </div>
      <div class="card">
        <div class="head"><h2>백업</h2><span class="hint">JSON Lines로 내려받기</span></div>
        <form id="export-form" style="grid-template-columns:1fr auto;">
          <input id="export-project" placeholder="project" required>
          <button type="submit">다운로드</button>
        </form>
      </div>
    </section>

    <!-- 도구 -->
    <section id="page-tools" class="page" hidden>
      <div class="section-head">
        <h1>도구</h1>
        <div class="lede">텍스트 줄이기, 실패 진단, 코드 시그니처, 템플릿 스캐폴드, 프롬프트.</div>
      </div>
      <nav class="subtabs" id="tool-subtabs">
        <a class="active" data-sub="compress">압축</a>
        <a data-sub="diagnose">진단</a>
        <a data-sub="repomap">코드 맵</a>
        <a data-sub="templates">템플릿</a>
        <a data-sub="prompts">프롬프트</a>
      </nav>

      <div id="sub-compress" class="subpage">
        <div class="card">
          <div class="head"><h2>텍스트 압축</h2><span class="hint">룰 엔진 또는 ML 휴리스틱</span></div>
          <form id="compress-form">
            <div class="row">
              <select id="compress-mode">
                <option value="rules">룰 엔진 (정규식)</option>
                <option value="ml">ML 휴리스틱 (토큰 중요도)</option>
              </select>
              <select id="compress-level">
                <option value="lite">lite (약함)</option>
                <option value="full" selected>full (기본)</option>
                <option value="ultra">ultra (강함)</option>
                <option value="extreme">extreme (최강)</option>
              </select>
            </div>
            <div class="row">
              <select id="compress-format">
                <option value="plain" selected>plain</option>
                <option value="markdown">markdown</option>
                <option value="xml">xml</option>
                <option value="json">json</option>
              </select>
              <input id="compress-ratio" type="number" step="0.05" min="0.1" max="1" value="0.5" title="ml ratio">
            </div>
            <textarea id="compress-input" rows="5" placeholder="줄일 텍스트를 붙여 넣으세요" required></textarea>
            <button class="primary" type="submit">압축</button>
          </form>
          <div id="compress-summary" class="out-meta"></div>
          <pre id="compress-output" hidden></pre>
        </div>
        <div class="card">
          <div class="head"><h2>명령 출력 필터</h2><span class="hint">git / cargo 같은 노이즈 줄이기</span></div>
          <form id="proxy-form">
            <div class="row">
              <select id="proxy-mode">
                <option value="command">command — 명령으로 자동 감지</option>
                <option value="errors_only">errors_only — 에러 줄만</option>
                <option value="ultra_compact">ultra_compact — 반복 묶음</option>
              </select>
              <input id="proxy-command" placeholder="명령 (예: git status)">
            </div>
            <input id="proxy-context" type="number" min="0" max="20" value="3" title="errors_only context lines">
            <textarea id="proxy-raw" rows="5" placeholder="원본 stdout / stderr 붙여넣기" required></textarea>
            <button class="primary" type="submit">필터링</button>
          </form>
          <div id="proxy-summary" class="out-meta"></div>
          <pre id="proxy-output" hidden></pre>
        </div>
      </div>

      <div id="sub-diagnose" class="subpage" hidden>
        <div class="card">
          <div class="head"><h2>실패 진단</h2><span class="hint">로그 → LLM이 원인 + 수정 한 줄로 답</span></div>
          <form id="diagnose-form">
            <div class="row">
              <input id="diagnose-model" placeholder="model (예: claude-haiku-4-5)" required>
              <input id="diagnose-context" type="number" min="0" max="20" value="3" title="context lines">
            </div>
            <textarea id="diagnose-raw" rows="8" placeholder="빌드 / 테스트 실패 로그" required></textarea>
            <button class="primary" type="submit">진단 요청</button>
          </form>
          <div id="diagnose-meta" class="out-meta"></div>
          <pre id="diagnose-output" hidden></pre>
        </div>
      </div>

      <div id="sub-repomap" class="subpage" hidden>
        <div class="card">
          <div class="head"><h2>코드 시그니처 맵</h2><span class="hint">Rust / Python / TypeScript 자동 감지</span></div>
          <form id="repomap-form">
            <input id="repomap-root" placeholder="프로젝트 경로 (예: /home/user/projects/foo)" required>
            <div class="row">
              <input id="repomap-ext" placeholder="확장자 (비우면 자동: .rs/.py/.ts)">
              <input id="repomap-max" type="number" min="1024" step="1024" value="524288" title="파일당 최대 바이트">
            </div>
            <button class="primary" type="submit">맵 생성</button>
          </form>
          <div id="repomap-summary" class="out-meta"></div>
          <pre id="repomap-output" style="max-height:420px;overflow:auto;" hidden></pre>
        </div>
      </div>

      <div id="sub-templates" class="subpage" hidden>
        <div class="card">
          <div class="head"><h2>프로젝트 템플릿</h2><span class="hint">개발 · 디자인 · 설계 카테고리</span></div>
          <table id="tpl"><thead><tr><th>이름</th><th>카테고리</th><th>설명</th><th></th></tr></thead><tbody><tr><td colspan="4" class="empty">불러오는 중…</td></tr></tbody></table>
        </div>
        <div class="card">
          <div class="head"><h2>새 프로젝트 생성</h2><span class="hint">미리보기는 디스크에 쓰지 않음</span></div>
          <form id="scaffold-form">
            <div class="row">
              <select id="scaffold-template" required></select>
              <input id="scaffold-target" placeholder="대상 디렉터리 (예: ./hello)" required>
            </div>
            <div id="scaffold-vars"></div>
            <label><input id="scaffold-overwrite" type="checkbox"> 기존 파일 덮어쓰기</label>
            <div style="display:flex;gap:0.5rem;">
              <button type="button" id="scaffold-preview" class="ghost">미리보기</button>
              <button type="submit" class="primary">생성</button>
            </div>
          </form>
          <div id="scaffold-result" class="out-meta"></div>
        </div>
      </div>

      <div id="sub-prompts" class="subpage" hidden>
        <div class="card">
          <div class="head"><h2>버전 프롬프트</h2><span class="hint">$RTRT_PROMPTS_DIR (기본 ~/.rtrt/prompts)</span></div>
          <table id="prompts-tbl"><thead><tr><th>이름</th><th>최신</th><th>버전</th></tr></thead><tbody><tr><td colspan="3" class="empty">아직 저장된 프롬프트 없음. CLI <code>rtrt prompt save</code> 로 추가하세요.</td></tr></tbody></table>
          <pre id="prompt-body" hidden></pre>
        </div>
      </div>
    </section>

    <!-- 설정 -->
    <section id="page-settings" class="page" hidden>
      <div class="section-head">
        <h1>설정</h1>
        <div class="lede">에이전트 연결 + 환경 정보.</div>
      </div>
      <nav class="subtabs" id="setting-subtabs">
        <a class="active" data-sub="connect">에이전트 연결</a>
        <a data-sub="env">환경</a>
      </nav>
      <div id="sub-connect" class="subpage">
        <div class="card">
          <div class="head"><h2>MCP 설정 스니펫 생성</h2><span class="hint">디스크에 쓰지 않음 — 복붙용 출력만</span></div>
          <form id="setup-form">
            <div class="row">
              <select id="setup-agent">
                <option value="claude-code">claude-code</option>
                <option value="cursor">cursor</option>
                <option value="codex">codex</option>
              </select>
              <input id="setup-memory" placeholder="memory 경로" value=".rtrt/memory.sqlite">
            </div>
            <input id="setup-binary" placeholder="rtrt-mcp 바이너리 경로 (선택)">
            <button class="primary" type="submit">스니펫 생성</button>
          </form>
          <pre id="setup-output" hidden></pre>
        </div>
      </div>
      <div id="sub-env" class="subpage" hidden>
        <div class="card">
          <div class="head"><h2>환경 정보</h2></div>
          <table id="env-tbl"><tbody>
            <tr><td>대시보드 바인드</td><td><code id="env-bind">—</code></td></tr>
            <tr><td>인증 토큰</td><td id="env-token">—</td></tr>
            <tr><td>응답 캐시</td><td id="env-cache">—</td></tr>
            <tr><td>예산 한도</td><td id="env-budget">—</td></tr>
          </tbody></table>
        </div>
      </div>
    </section>
  </main>
</div>

<div class="activity">
  <span class="dot-pulse"></span>
  <span class="feed" id="activity-feed">대시보드 부팅 완료. 좌측 메뉴에서 작업을 시작하세요.</span>
</div>

<script>
(function initTheme() {
  const saved = localStorage.getItem('rtrt-theme');
  const prefersDark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
  document.documentElement.setAttribute('data-theme', saved || (prefersDark ? 'dark' : 'light'));
})();
document.getElementById('theme-toggle').onclick = () => {
  const next = (document.documentElement.getAttribute('data-theme') === 'dark') ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('rtrt-theme', next);
};

// Sidebar nav
document.querySelectorAll('aside a.nav').forEach(a => a.onclick = () => {
  document.querySelectorAll('aside a.nav').forEach(x => x.classList.remove('active'));
  document.querySelectorAll('.page').forEach(x => x.hidden = true);
  a.classList.add('active');
  document.getElementById('page-' + a.dataset.page).hidden = false;
});

// Sub-tabs (도구, 설정)
function wireSubtabs(navId) {
  const nav = document.getElementById(navId);
  if (!nav) return;
  nav.querySelectorAll('a').forEach(a => a.onclick = () => {
    nav.querySelectorAll('a').forEach(x => x.classList.remove('active'));
    a.classList.add('active');
    const parent = nav.parentElement;
    parent.querySelectorAll('.subpage').forEach(x => x.hidden = true);
    document.getElementById('sub-' + a.dataset.sub).hidden = false;
  });
}
wireSubtabs('tool-subtabs');
wireSubtabs('setting-subtabs');

// Activity feed
const FEED = [];
function pushActivity(msg) {
  const t = new Date().toTimeString().slice(0, 8);
  FEED.unshift(`${t} · ${msg}`);
  if (FEED.length > 12) FEED.pop();
  document.getElementById('activity-feed').textContent = FEED[0] || '';
}

// Utility
function fmtUsd(v) { return v === null || v === undefined ? '—' : `$${Number(v).toFixed(4)}`; }
function setPill(id, ok, text) {
  const el = document.getElementById(id);
  el.classList.remove('ok', 'warn');
  el.classList.add(ok === true ? 'ok' : ok === false ? 'warn' : '');
  if (text) el.lastChild.textContent = ' ' + text;
}
function animateCount(el, target) {
  if (!el) return;
  const start = Number(el.dataset.target || 0);
  el.dataset.target = target;
  const startAt = performance.now();
  const duration = 600;
  const tick = (now) => {
    const t = Math.min(1, (now - startAt) / duration);
    const eased = 1 - Math.pow(1 - t, 3);
    const value = Math.round(start + (target - start) * eased);
    el.textContent = value.toLocaleString();
    if (t < 1) requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}
function spark(svgId, values, color) {
  const svg = document.getElementById(svgId);
  if (!svg) return;
  if (!values.length) { svg.innerHTML = `<text x="50%" y="55%" text-anchor="middle" fill="var(--muted)" font-size="11">데이터 없음</text>`; return; }
  const w = 400, h = 64, pad = 4;
  const max = Math.max(1, ...values);
  const step = values.length > 1 ? (w - pad*2) / (values.length - 1) : 0;
  const pts = values.map((v, i) => {
    const x = pad + i * step;
    const y = h - pad - (v/max) * (h - pad*2);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  }).join(' ');
  const fill = `${pad},${h-pad} ${pts} ${(pad+(values.length-1)*step).toFixed(1)},${h-pad}`;
  svg.innerHTML =
    `<defs><linearGradient id="g-${svgId}" x1="0" y1="0" x2="0" y2="1">` +
    `<stop offset="0%" stop-color="${color}" stop-opacity="0.35"/>` +
    `<stop offset="100%" stop-color="${color}" stop-opacity="0"/></linearGradient></defs>` +
    `<polygon points="${fill}" fill="url(#g-${svgId})"/>` +
    `<polyline points="${pts}" fill="none" stroke="${color}" stroke-width="1.5"/>` +
    `<text x="${w-pad}" y="11" text-anchor="end" fill="var(--muted)" font-size="10">max ${max}</text>`;
}

async function loadOverview() {
  const [mRes, bRes] = await Promise.all([
    fetch('/api/metrics').then(r => r.ok ? r.json() : ({summary:{}, recent:[]})).catch(() => ({summary:{}, recent:[]})),
    fetch('/api/budget').then(r => r.ok ? r.json() : null).catch(() => null),
  ]);
  const s = mRes.summary || {};
  const calls = s.calls || 0;
  const saved = (s.total_input_tokens || 0) + (s.total_output_tokens || 0);
  const avgLatency = calls ? (s.total_latency_ms / Math.max(1, calls)).toFixed(0) : '—';
  animateCount(document.getElementById('kpi-saved'), saved);
  animateCount(document.getElementById('kpi-calls'), calls);
  document.getElementById('kpi-latency').textContent = `${avgLatency} ms`;
  document.getElementById('kpi-spent').textContent = bRes ? fmtUsd(bRes.spent_usd) : '—';
  document.getElementById('kpi-spent-sub').textContent = bRes && bRes.cap_usd ? `한도 ${fmtUsd(bRes.cap_usd)}` : '한도 미설정';

  setPill('pill-gateway', calls > 0, calls > 0 ? '게이트웨이 활성' : '게이트웨이 대기');
  setPill('pill-memory', null, '메모리');
  if (bRes) {
    const cache = (bRes.cache_len === null || bRes.cache_len === undefined) ? 'off' : `${bRes.cache_len}건`;
    setPill('pill-cache', bRes.cache_len !== null && bRes.cache_len !== undefined, `캐시 ${cache}`);
    document.getElementById('env-cache').textContent = cache;
    document.getElementById('env-budget').textContent = bRes.cap_usd ? `${fmtUsd(bRes.cap_usd)} (${fmtUsd(bRes.spent_usd)} 사용)` : '미설정';
  }

  const recent = mRes.recent || [];
  spark('chart-latency', recent.slice().reverse().map(r => r.latency_ms), '#2962FF');
  spark('chart-tokens', recent.slice().reverse().map(r => (r.usage.input_tokens || 0) + (r.usage.output_tokens || 0)), '#16a34a');

  const byParent = new Map(); const heads = [];
  for (const r of recent) {
    if (r.parent_id) {
      let arr = byParent.get(r.parent_id);
      if (!arr) { arr = []; byParent.set(r.parent_id, arr); }
      arr.push(r);
    } else { heads.push(r); }
  }
  function row(r, depth) {
    const t = new Date(r.started_at * 1000).toTimeString().slice(0, 8);
    const status = r.ok ? '<span class="badge ok">ok</span>' : `<span class="badge err">${r.error || 'failed'}</span>`;
    const ind = depth ? `<span style="color:var(--muted);">└─ </span>` : '';
    return `<tr><td>${ind}${t}</td><td>${r.provider}</td><td><code>${r.model}</code></td><td>${r.usage.input_tokens}</td><td>${r.usage.output_tokens}</td><td>${r.latency_ms} ms</td><td>${status}</td></tr>`;
  }
  const rows = [];
  for (const h of heads) {
    rows.push(row(h, 0));
    for (const c of (byParent.get(h.id) || [])) rows.push(row(c, 1));
  }
  document.querySelector('#recent-tbl tbody').innerHTML = rows.join('') || '<tr><td colspan="7" class="empty">아직 호출 없음. <code>rtrt provider chat</code> 또는 MCP로 시작하세요.</td></tr>';
}
document.getElementById('overview-refresh').onclick = () => { loadOverview(); pushActivity('개요 새로고침'); };

// Memory
document.getElementById('recall-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    project: document.getElementById('recall-project').value,
    query: document.getElementById('recall-query').value,
    limit: Number(document.getElementById('recall-limit').value) || 10,
    filter: document.getElementById('recall-filter').value || null,
  };
  const tbody = document.querySelector('#recall-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="4" class="empty">검색 중…</td></tr>`;
  const r = await fetch('/api/memory/recall', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { tbody.innerHTML = `<tr><td colspan="4" class="empty" style="color:var(--err);">${r.status}: ${await r.text()}</td></tr>`; return; }
  const d = await r.json();
  tbody.innerHTML = d.hits.length
    ? d.hits.map(h => `<tr><td>${h.id}</td><td><code>${h.kind}</code></td><td>${h.scope}</td><td>${h.body.replace(/</g,'&lt;')}</td></tr>`).join('')
    : `<tr><td colspan="4" class="empty">결과 없음. 다른 검색어를 시도해보세요.</td></tr>`;
  pushActivity(`recall ${body.project} · ${d.hits.length}건`);
};
document.getElementById('save-form').onsubmit = async (ev) => {
  ev.preventDefault();
  let metadata = {};
  const raw = document.getElementById('save-metadata').value.trim();
  if (raw) { try { metadata = JSON.parse(raw); } catch (e) { document.getElementById('save-result').innerHTML = `<span style="color:var(--err);">JSON 파싱 실패: ${e}</span>`; return; } }
  const body = {
    project: document.getElementById('save-project').value,
    kind: document.getElementById('save-kind').value || 'note',
    body: document.getElementById('save-body').value,
    metadata,
  };
  const r = await fetch('/api/memory/save', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  const out = document.getElementById('save-result');
  if (!r.ok) { out.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">✓ 저장 id=${d.id}</span>`;
  pushActivity(`save ${body.project} · id=${d.id}`);
};
document.getElementById('blocks-list-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = document.getElementById('blocks-project').value;
  const tbody = document.querySelector('#blocks-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="2" class="empty">불러오는 중…</td></tr>`;
  const r = await fetch(`/api/memory/blocks?project=${encodeURIComponent(project)}`);
  if (!r.ok) { tbody.innerHTML = `<tr><td colspan="2" class="empty" style="color:var(--err);">${r.status}: ${await r.text()}</td></tr>`; return; }
  const d = await r.json();
  tbody.innerHTML = d.blocks.length
    ? d.blocks.map(b => `<tr><td><code>${b.kind.replace(/^block:/,'')}</code></td><td>${b.body.replace(/</g,'&lt;')}</td></tr>`).join('')
    : `<tr><td colspan="2" class="empty">블록 없음. 아래에서 추가하세요.</td></tr>`;
};
document.getElementById('blocks-set-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    project: document.getElementById('block-set-project').value,
    name: document.getElementById('block-set-name').value,
    body: document.getElementById('block-set-body').value,
  };
  const r = await fetch('/api/memory/blocks', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  const out = document.getElementById('block-set-result');
  if (!r.ok) { out.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">✓ 저장 id=${d.id}</span>`;
};
document.getElementById('export-form').onsubmit = (ev) => {
  ev.preventDefault();
  const project = document.getElementById('export-project').value;
  window.location.href = `/api/memory/export?project=${encodeURIComponent(project)}`;
  pushActivity(`export ${project}`);
};

// Tools — compress
document.getElementById('compress-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    text: document.getElementById('compress-input').value,
    ml: document.getElementById('compress-mode').value === 'ml',
    level: document.getElementById('compress-level').value,
    format: document.getElementById('compress-format').value,
    ratio: Number(document.getElementById('compress-ratio').value),
  };
  const sum = document.getElementById('compress-summary');
  const pre = document.getElementById('compress-output');
  sum.textContent = '압축 중…'; pre.hidden = true;
  const r = await fetch('/api/compress', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const ratio = d.original_len ? ((d.compressed_len / d.original_len) * 100).toFixed(1) : '0';
  sum.innerHTML = `<span class="badge ok">✓ ${d.original_len} → ${d.compressed_len} (${ratio}%)</span> <span style="margin-left:0.5rem;">절감 ${d.saved_chars}자 · mode=${d.mode}${d.scorer ? ` · ${d.scorer}` : ''}</span>`;
  pre.hidden = false; pre.textContent = d.compressed;
  pushActivity(`compress · -${d.saved_chars}자`);
};
document.getElementById('proxy-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    mode: document.getElementById('proxy-mode').value,
    command: document.getElementById('proxy-command').value || null,
    raw: document.getElementById('proxy-raw').value,
    context: Number(document.getElementById('proxy-context').value) || 3,
  };
  const sum = document.getElementById('proxy-summary');
  const pre = document.getElementById('proxy-output');
  sum.textContent = '필터링 중…'; pre.hidden = true;
  const r = await fetch('/api/proxy', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const ratio = d.original_len ? ((d.filtered_len / d.original_len) * 100).toFixed(1) : '0';
  sum.innerHTML = `<span class="badge ok">✓ ${d.original_len} → ${d.filtered_len} (${ratio}%)</span> <span style="margin-left:0.5rem;">절감 ${d.saved_chars}자 · mode=${d.mode}</span>`;
  pre.hidden = false; pre.textContent = d.filtered;
};
document.getElementById('diagnose-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    model: document.getElementById('diagnose-model').value,
    raw: document.getElementById('diagnose-raw').value,
    context: Number(document.getElementById('diagnose-context').value) || 3,
  };
  const meta = document.getElementById('diagnose-meta');
  const pre = document.getElementById('diagnose-output');
  meta.textContent = '진단 중…'; pre.hidden = true;
  const r = await fetch('/api/diagnose', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { meta.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  meta.innerHTML = `<span class="badge ok">${d.provider}/${d.model}</span> in ${d.input_tokens} · out ${d.output_tokens}`;
  pre.hidden = false; pre.textContent = d.diagnosis;
  pushActivity(`diagnose · ${d.provider}`);
};
document.getElementById('repomap-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    root: document.getElementById('repomap-root').value,
    ext: document.getElementById('repomap-ext').value || '',
    max_bytes: Number(document.getElementById('repomap-max').value) || 524288,
  };
  const sum = document.getElementById('repomap-summary');
  const pre = document.getElementById('repomap-output');
  sum.textContent = '스캔 중…'; pre.hidden = true;
  const r = await fetch('/api/repo-map', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  sum.innerHTML = `<span class="badge ok">${d.files} 파일</span> ${d.total_bytes} bytes 스캔 · 시그니처 ${d.signature_chars} chars`;
  pre.hidden = false;
  pre.textContent = d.entries.map(e => `// ${e.path} [${e.language || ''}]\n${e.signatures}\n`).join('\n');
};

// Templates
const CAT_LABEL = { development: '개발', design: '디자인', planning: '설계' };
let LOADED_TEMPLATES = [];
async function loadTemplates() {
  const tpls = await fetch('/api/templates').then(r => r.ok ? r.json() : []).catch(() => []);
  LOADED_TEMPLATES = tpls;
  const tbody = document.querySelector('#tpl tbody');
  if (!tpls.length) { tbody.innerHTML = '<tr><td colspan="4" class="empty">템플릿 없음</td></tr>'; return; }
  const order = ['development', 'design', 'planning'];
  const grouped = {};
  for (const t of tpls) { (grouped[t.category || 'development'] ||= []).push(t); }
  tbody.innerHTML = order.flatMap(cat => (grouped[cat] || []).map(t =>
    `<tr><td><code>${t.name}</code></td><td><span class="badge">${CAT_LABEL[cat]}</span></td><td>${t.description}</td><td><a data-pick="${t.name}">사용</a></td></tr>`
  )).join('');
  document.querySelectorAll('a[data-pick]').forEach(a => a.onclick = () => {
    document.getElementById('scaffold-template').value = a.dataset.pick;
    renderScaffoldVars();
    document.querySelector('#scaffold-target').focus();
  });
  const sel = document.getElementById('scaffold-template');
  sel.innerHTML = tpls.map(t => `<option value="${t.name}">[${CAT_LABEL[t.category || 'development']}] ${t.name}</option>`).join('');
  sel.onchange = renderScaffoldVars;
  renderScaffoldVars();
}
function renderScaffoldVars() {
  const name = document.getElementById('scaffold-template').value;
  const tpl = LOADED_TEMPLATES.find(t => t.name === name);
  const wrap = document.getElementById('scaffold-vars');
  if (!tpl) { wrap.innerHTML = ''; return; }
  wrap.innerHTML = `<table style="margin-top:0.5rem;"><tbody>` +
    tpl.variables.map(v =>
      `<tr><td style="width:30%;"><code>${v.name}</code>${v.required ? ' *' : ''}</td>` +
      `<td><input data-var="${v.name}" placeholder="${v.description || ''}" value="${v.default || ''}" style="width:100%;"></td></tr>`
    ).join('') + `</tbody></table>`;
}
function buildScaffoldBody() {
  const variables = {};
  document.querySelectorAll('#scaffold-vars [data-var]').forEach(inp => {
    if (inp.value) variables[inp.dataset.var] = inp.value;
  });
  return {
    template: document.getElementById('scaffold-template').value,
    target: document.getElementById('scaffold-target').value,
    variables,
    overwrite: document.getElementById('scaffold-overwrite').checked,
  };
}
document.getElementById('scaffold-preview').onclick = async () => {
  const out = document.getElementById('scaffold-result');
  out.textContent = '미리보기…';
  const r = await fetch('/api/templates/scaffold/preview', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(buildScaffoldBody())});
  if (!r.ok) { out.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const rows = d.files.map(f => `<tr><td><code>${f.path}</code></td><td>${f.bytes}</td></tr>`).join('');
  out.innerHTML = `<div>${d.root} 아래 ${d.files.length} 파일 생성 예정</div><table style="margin-top:0.4rem;"><thead><tr><th>경로</th><th>bytes</th></tr></thead><tbody>${rows}</tbody></table>`;
};
document.getElementById('scaffold-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const out = document.getElementById('scaffold-result');
  out.textContent = '생성 중…';
  const r = await fetch('/api/templates/scaffold', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(buildScaffoldBody())});
  if (!r.ok) { out.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  out.innerHTML = `<span class="badge ok">✓ ${d.files_written} 파일</span> <code>${d.root}</code>` +
    (d.post_hooks.length ? `<br>후처리: ${d.post_hooks.map(h => `<code>${h}</code>`).join(', ')}` : '');
  pushActivity(`scaffold · ${d.files_written} 파일`);
};

// Prompts
async function loadPrompts() {
  let prompts = [];
  try { prompts = await fetch('/api/prompts').then(r => r.ok ? r.json() : []); } catch (_) {}
  const tbody = document.querySelector('#prompts-tbl tbody');
  if (!prompts.length) return;
  tbody.innerHTML = prompts.map(p => {
    const versions = p.versions.map(v => `<a data-name="${p.name}" data-version="${v}" style="margin-right:0.5rem;">v${v}</a>`).join('');
    return `<tr><td><code>${p.name}</code></td><td>v${p.latest}</td><td>${versions}</td></tr>`;
  }).join('');
  tbody.querySelectorAll('a[data-name]').forEach(a => a.onclick = async () => {
    const body = await fetch(`/api/prompts/${a.dataset.name}/${a.dataset.version}`).then(r => r.json());
    const pre = document.getElementById('prompt-body');
    pre.hidden = false;
    pre.textContent = `# ${body.name} v${body.version}\n\n${body.body}`;
  });
}

// Settings
document.getElementById('setup-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    agent: document.getElementById('setup-agent').value,
    memory: document.getElementById('setup-memory').value || null,
    binary: document.getElementById('setup-binary').value || null,
  };
  const pre = document.getElementById('setup-output');
  pre.hidden = true;
  const r = await fetch('/api/setup', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { pre.hidden = false; pre.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  pre.hidden = false;
  pre.textContent = `# ${d.agent} → ${d.target_path}\n\n${d.snippet}`;
};

// Init
document.getElementById('env-bind').textContent = window.location.host;
loadOverview();
loadTemplates();
loadPrompts();
setInterval(loadOverview, 5000);
pushActivity('대시보드 부팅 완료. 좌측 메뉴에서 작업을 시작하세요.');
</script>
</body>
</html>
"#;
