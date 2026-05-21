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
<html lang="en">
<head>
<meta charset="utf-8">
<title>RTRT</title>
<style>
  :root {
    --bg: #ffffff; --fg: #1a1a1a; --muted: #666; --border: #e6e6e6;
    --card: #fafafa; --code-bg: #f3f3f3; --accent: #2962FF; --err: #c0392b;
  }
  :root[data-theme="dark"] {
    --bg: #15171a; --fg: #e6e6e6; --muted: #9aa0a6; --border: #2a2d31;
    --card: #1c1f23; --code-bg: #23272e; --accent: #6aa3ff; --err: #ff6b6b;
  }
  body { font: 14px/1.5 system-ui, sans-serif; max-width: 880px; margin: 1.5rem auto; padding: 0 1rem; background: var(--bg); color: var(--fg); }
  header { display:flex; align-items:baseline; gap:1rem; margin-bottom: 1rem; }
  header h1 { margin: 0; font-size: 1.6rem; }
  header .sub { color: var(--muted); flex: 1; }
  #theme-toggle { cursor: pointer; background: transparent; border: 1px solid var(--border); color: var(--fg); padding: 0.25rem 0.6rem; border-radius: 6px; font-size: 0.9em; }
  nav.tabs { display:flex; gap:0.5rem; border-bottom: 1px solid var(--border); margin-bottom: 1.5rem; }
  nav.tabs a { cursor: pointer; padding: 0.5rem 0.9rem; border-radius: 6px 6px 0 0; color: var(--muted); }
  nav.tabs a.active { color: var(--fg); background: var(--card); border: 1px solid var(--border); border-bottom-color: var(--card); margin-bottom: -1px; }
  section.tab { display: none; }
  section.tab.active { display: block; }
  h2 { margin: 0 0 0.5rem; font-size: 1.15rem; }
  h3 { margin: 1.25rem 0 0.4rem; font-size: 1rem; }
  .card { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }
  .card .hint { color: var(--muted); font-size: 0.9em; margin-bottom: 0.5rem; }
  .kpis { display: grid; grid-template-columns: repeat(auto-fit, minmax(110px, 1fr)); gap: 0.75rem; }
  .kpi { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 0.6rem 0.75rem; }
  .kpi .label { color: var(--muted); font-size: 0.8em; }
  .kpi .value { font-size: 1.3rem; font-weight: 600; }
  table { width: 100%; border-collapse: collapse; }
  th, td { padding: 0.4rem 0.6rem; border-bottom: 1px solid var(--border); text-align: left; vertical-align: top; }
  code { background: var(--code-bg); padding: 0 0.25rem; border-radius: 3px; font-family: ui-monospace, monospace; font-size: 0.9em; }
  pre { background: var(--code-bg); border: 1px solid var(--border); padding: 0.75rem; border-radius: 6px; white-space: pre-wrap; }
  form { display: grid; gap: 0.5rem; }
  form .row { display: grid; grid-template-columns: 1fr 1fr; gap: 0.5rem; }
  input, textarea, select, button { background: var(--bg); color: var(--fg); border: 1px solid var(--border); padding: 0.4rem 0.6rem; border-radius: 6px; font: inherit; }
  button { cursor: pointer; }
  button.primary { background: var(--accent); color: white; border-color: var(--accent); }
  details { margin-bottom: 0.6rem; }
  details > summary { cursor: pointer; padding: 0.5rem 0.75rem; background: var(--card); border: 1px solid var(--border); border-radius: 6px; font-weight: 500; }
  details[open] > summary { border-radius: 6px 6px 0 0; }
  details > div { border: 1px solid var(--border); border-top: 0; border-radius: 0 0 6px 6px; padding: 0.9rem; }
  .err { color: var(--err); }
  .muted { color: var(--muted); font-size: 0.9em; }
  svg.spark { width: 100%; height: 60px; }
</style>
</head>
<body>
<header>
  <h1>RTRT</h1>
  <span class="sub">웹 콘솔</span>
  <button id="theme-toggle" title="다크 / 라이트 토글">🌗</button>
</header>

<nav class="tabs">
  <a data-tab="overview" class="active">개요</a>
  <a data-tab="memory">메모리</a>
  <a data-tab="tools">도구</a>
</nav>

<section id="overview" class="tab active">
  <div class="card">
    <h2>현재 상태</h2>
    <div class="hint">게이트웨이 요청 / 토큰 / 예산 / 캐시 요약. 5초마다 자동 새로고침.</div>
    <div id="kpi-grid" class="kpis">불러오는 중…</div>
  </div>
  <div class="card">
    <h2>응답 속도 + 토큰 추이</h2>
    <div class="hint">최근 요청 50건 기준.</div>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:1rem;">
      <div>
        <div class="muted">응답 속도 (ms)</div>
        <svg id="chart-latency" class="spark" viewBox="0 0 400 60" preserveAspectRatio="none"></svg>
      </div>
      <div>
        <div class="muted">토큰 (in + out)</div>
        <svg id="chart-tokens" class="spark" viewBox="0 0 400 60" preserveAspectRatio="none"></svg>
      </div>
    </div>
  </div>
  <div class="card">
    <h2>최근 요청</h2>
    <table id="recent-tbl"><thead><tr><th>시각</th><th>프로바이더</th><th>모델</th><th>in</th><th>out</th><th>지연</th><th>상태</th></tr></thead><tbody></tbody></table>
  </div>
</section>

<section id="memory" class="tab">
  <div class="card">
    <h2>검색 (recall)</h2>
    <div class="hint">BM25 검색. 페이로드 필터 예: <code>source=claude,topic~^auth</code></div>
    <form id="recall-form">
      <div class="row">
        <input id="recall-project" placeholder="project" required>
        <input id="recall-limit" type="number" min="1" max="50" value="10">
      </div>
      <input id="recall-query" placeholder="검색어" required>
      <input id="recall-filter" placeholder="필터 (선택)">
      <button class="primary" type="submit">검색</button>
    </form>
    <table id="recall-tbl" style="margin-top:0.75rem;"><thead><tr><th>id</th><th>kind</th><th>scope</th><th>body</th></tr></thead><tbody></tbody></table>
  </div>

  <div class="card">
    <h2>새 메모리 저장</h2>
    <form id="save-form">
      <div class="row">
        <input id="save-project" placeholder="project" required>
        <input id="save-kind" placeholder="kind (기본 note)" value="note">
      </div>
      <textarea id="save-body" rows="3" placeholder="내용" required></textarea>
      <input id="save-metadata" placeholder='메타데이터 JSON (선택, 예: {"source":"claude"})'>
      <button class="primary" type="submit">저장</button>
    </form>
    <div id="save-result" class="muted" style="margin-top:0.5rem;"></div>
  </div>

  <div class="card">
    <h2>블록 (persona / human / context)</h2>
    <div class="hint">에이전트 페르소나, 사용자 정보, 컨텍스트 같은 영속 슬롯.</div>
    <form id="blocks-list-form" style="grid-template-columns: 1fr auto;">
      <input id="blocks-project" placeholder="project" required>
      <button type="submit">목록</button>
    </form>
    <table id="blocks-tbl" style="margin-top:0.5rem;"><thead><tr><th>이름</th><th>내용</th></tr></thead><tbody></tbody></table>
    <h3>블록 저장 / 덮어쓰기</h3>
    <form id="blocks-set-form">
      <div class="row">
        <input id="block-set-name" placeholder="이름 (persona / human / context …)" required>
        <input id="block-set-project" placeholder="project" required>
      </div>
      <textarea id="block-set-body" rows="2" placeholder="블록 내용" required></textarea>
      <button class="primary" type="submit">저장</button>
    </form>
    <div id="block-set-result" class="muted" style="margin-top:0.5rem;"></div>
  </div>

  <div class="card">
    <h2>백업</h2>
    <form id="export-form" style="grid-template-columns: 1fr auto;">
      <input id="export-project" placeholder="project" required>
      <button type="submit">JSONL 다운로드</button>
    </form>
  </div>
</section>

<section id="tools" class="tab">
  <details open>
    <summary>압축 (compress)</summary>
    <div>
      <div class="muted">텍스트를 룰 엔진(lite/full/ultra/extreme) 또는 ML 휴리스틱으로 줄임.</div>
      <form id="compress-form" style="margin-top:0.5rem;">
        <div class="row">
          <select id="compress-mode">
            <option value="rules">룰 엔진</option>
            <option value="ml">ML 휴리스틱</option>
          </select>
          <select id="compress-level">
            <option value="lite">lite</option>
            <option value="full" selected>full</option>
            <option value="ultra">ultra</option>
            <option value="extreme">extreme</option>
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
        <textarea id="compress-input" rows="5" placeholder="원본 텍스트" required></textarea>
        <button class="primary" type="submit">압축</button>
      </form>
      <div id="compress-summary" class="muted" style="margin-top:0.5rem;"></div>
      <pre id="compress-output" style="display:none;"></pre>
    </div>
  </details>

  <details>
    <summary>명령 출력 필터 (proxy)</summary>
    <div>
      <div class="muted">git / cargo 같은 노이즈 많은 출력을 한 줄로 줄임. errors_only / ultra_compact 모드도 가능.</div>
      <form id="proxy-form" style="margin-top:0.5rem;">
        <div class="row">
          <select id="proxy-mode">
            <option value="command">command (자동 감지)</option>
            <option value="errors_only">errors_only</option>
            <option value="ultra_compact">ultra_compact</option>
          </select>
          <input id="proxy-command" placeholder="명령 (예: git status)">
        </div>
        <input id="proxy-context" type="number" min="0" max="20" value="3" title="errors_only context lines">
        <textarea id="proxy-raw" rows="5" placeholder="원본 stdout/stderr" required></textarea>
        <button class="primary" type="submit">필터링</button>
      </form>
      <div id="proxy-summary" class="muted" style="margin-top:0.5rem;"></div>
      <pre id="proxy-output" style="display:none;"></pre>
    </div>
  </details>

  <details>
    <summary>실패 진단 (diagnose)</summary>
    <div>
      <div class="muted">빌드 / 테스트 실패 로그를 LLM에 넘겨 원인 + 수정 제안을 한 번에 받음.</div>
      <form id="diagnose-form" style="margin-top:0.5rem;">
        <div class="row">
          <input id="diagnose-model" placeholder="model (예: claude-haiku-4-5)" required>
          <input id="diagnose-context" type="number" min="0" max="20" value="3">
        </div>
        <textarea id="diagnose-raw" rows="6" placeholder="실패 로그" required></textarea>
        <button class="primary" type="submit">진단</button>
      </form>
      <div id="diagnose-meta" class="muted" style="margin-top:0.5rem;"></div>
      <pre id="diagnose-output" style="display:none;"></pre>
    </div>
  </details>

  <details>
    <summary>코드 시그니처 맵 (repo-map)</summary>
    <div>
      <div class="muted">Rust / Python / TypeScript 프로젝트의 함수·클래스 시그니처만 추출. 본문 제거.</div>
      <form id="repomap-form" style="margin-top:0.5rem;">
        <input id="repomap-root" placeholder="프로젝트 경로" required>
        <div class="row">
          <input id="repomap-ext" placeholder="확장자 (선택, 비우면 자동)" >
          <input id="repomap-max" type="number" min="1024" step="1024" value="524288" title="파일당 최대 바이트">
        </div>
        <button class="primary" type="submit">맵 생성</button>
      </form>
      <div id="repomap-summary" class="muted" style="margin-top:0.5rem;"></div>
      <pre id="repomap-output" style="display:none; max-height: 360px; overflow:auto;"></pre>
    </div>
  </details>

  <details>
    <summary>프로젝트 스캐폴드 (templates)</summary>
    <div>
      <div class="muted">개발 / 디자인 / 설계 카테고리별 빌트인 + 커스텀 템플릿. 미리보기는 디스크 미기록.</div>
      <table id="tpl" style="margin-top:0.5rem;"><thead><tr><th>이름</th><th>카테고리</th><th>설명</th><th></th></tr></thead><tbody></tbody></table>
      <h3>스캐폴드 실행</h3>
      <form id="scaffold-form">
        <div class="row">
          <select id="scaffold-template" required></select>
          <input id="scaffold-target" placeholder="대상 디렉터리" required>
        </div>
        <div id="scaffold-vars"></div>
        <label><input id="scaffold-overwrite" type="checkbox"> 기존 파일 덮어쓰기</label>
        <div style="display:flex;gap:0.5rem;">
          <button type="button" id="scaffold-preview">미리보기</button>
          <button type="submit" class="primary">생성</button>
        </div>
      </form>
      <div id="scaffold-result" class="muted" style="margin-top:0.5rem;"></div>
    </div>
  </details>

  <details>
    <summary>프롬프트 (prompts)</summary>
    <div>
      <div class="muted">버전 관리되는 프롬프트 저장소. <code>$RTRT_PROMPTS_DIR</code> (기본 <code>~/.rtrt/prompts</code>).</div>
      <table id="prompts-tbl" style="margin-top:0.5rem;"><thead><tr><th>이름</th><th>최신</th><th>버전</th></tr></thead><tbody></tbody></table>
      <pre id="prompt-body" style="display:none; margin-top:0.5rem;"></pre>
    </div>
  </details>

  <details>
    <summary>에이전트 연결 (setup)</summary>
    <div>
      <div class="muted">Claude Code / Cursor / Codex MCP 설정 스니펫 생성 (디스크 미기록).</div>
      <form id="setup-form" style="margin-top:0.5rem;">
        <div class="row">
          <select id="setup-agent">
            <option value="claude-code">claude-code</option>
            <option value="cursor">cursor</option>
            <option value="codex">codex</option>
          </select>
          <input id="setup-memory" placeholder="memory 경로" value=".rtrt/memory.sqlite">
        </div>
        <input id="setup-binary" placeholder="rtrt-mcp 바이너리 경로 (선택)">
        <button type="submit" class="primary">스니펫 생성</button>
      </form>
      <pre id="setup-output" style="display:none; margin-top:0.5rem;"></pre>
    </div>
  </details>
</section>

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
document.querySelectorAll('nav.tabs a').forEach(a => a.onclick = () => {
  document.querySelectorAll('nav.tabs a').forEach(x => x.classList.remove('active'));
  document.querySelectorAll('section.tab').forEach(x => x.classList.remove('active'));
  a.classList.add('active');
  document.getElementById(a.dataset.tab).classList.add('active');
});

function fmtUsd(v) { return v === null || v === undefined ? '—' : `$${Number(v).toFixed(4)}`; }
function spark(svgId, values, color) {
  const svg = document.getElementById(svgId);
  if (!svg) return;
  if (!values.length) { svg.innerHTML = `<text x="50%" y="55%" text-anchor="middle" fill="var(--muted)" font-size="11">데이터 없음</text>`; return; }
  const w = 400, h = 60, pad = 4;
  const max = Math.max(1, ...values);
  const step = values.length > 1 ? (w - pad*2) / (values.length - 1) : 0;
  const pts = values.map((v, i) => {
    const x = pad + i*step;
    const y = h - pad - (v/max) * (h - pad*2);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  }).join(' ');
  const fill = `${pad},${h-pad} ${pts} ${(pad+(values.length-1)*step).toFixed(1)},${h-pad}`;
  svg.innerHTML =
    `<polygon points="${fill}" fill="${color}" fill-opacity="0.18"/>` +
    `<polyline points="${pts}" fill="none" stroke="${color}" stroke-width="1.5"/>` +
    `<text x="${w-pad}" y="10" text-anchor="end" fill="var(--muted)" font-size="10">max ${max}</text>`;
}

async function loadOverview() {
  const [m, b] = await Promise.all([
    fetch('/api/metrics').then(r => r.json()).catch(() => ({summary:{}, recent:[]})),
    fetch('/api/budget').then(r => r.ok ? r.json() : null).catch(() => null),
  ]);
  const s = m.summary || {};
  const calls = s.calls || 0;
  const avgLatency = calls ? (s.total_latency_ms/Math.max(1,calls)).toFixed(0) : '—';
  const cacheLen = (b && b.cache_len !== null && b.cache_len !== undefined) ? b.cache_len : 'off';
  const kpis = [
    ['호출 수', calls],
    ['성공 / 실패', `${s.successes || 0} / ${s.failures || 0}`],
    ['평균 지연', `${avgLatency} ms`],
    ['입력 토큰', s.total_input_tokens || 0],
    ['출력 토큰', s.total_output_tokens || 0],
    ['예산 사용', b ? fmtUsd(b.spent_usd) : '—'],
    ['예산 한도', b ? fmtUsd(b.cap_usd) : '—'],
    ['응답 캐시', cacheLen],
  ];
  document.getElementById('kpi-grid').innerHTML = kpis.map(([k,v]) =>
    `<div class="kpi"><div class="label">${k}</div><div class="value">${v}</div></div>`
  ).join('');
  const recent = (m.recent || []).slice();
  spark('chart-latency', recent.slice().reverse().map(r => r.latency_ms), 'var(--accent)');
  spark('chart-tokens', recent.slice().reverse().map(r => (r.usage.input_tokens||0)+(r.usage.output_tokens||0)), '#2ecc71');
  const byParent = new Map(); const heads = [];
  for (const r of recent) {
    if (r.parent_id) { (byParent.get(r.parent_id) || byParent.set(r.parent_id, []).get(r.parent_id)).push(r); }
    else { heads.push(r); }
  }
  function row(r, depth) {
    const t = new Date(r.started_at * 1000).toISOString().slice(11,19);
    const status = r.ok ? 'ok' : `<span class="err">${r.error||'failed'}</span>`;
    const ind = depth ? `<span class="muted">└─ </span>` : '';
    return `<tr><td>${ind}${t}</td><td>${r.provider}</td><td><code>${r.model}</code></td><td>${r.usage.input_tokens}</td><td>${r.usage.output_tokens}</td><td>${r.latency_ms} ms</td><td>${status}</td></tr>`;
  }
  const rows = [];
  for (const h of heads) {
    rows.push(row(h, 0));
    for (const c of (byParent.get(h.id) || [])) rows.push(row(c, 1));
  }
  document.querySelector('#recent-tbl tbody').innerHTML = rows.join('') || '<tr><td colspan="7" class="muted">아직 요청 없음</td></tr>';
}

document.getElementById('recall-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    project: document.getElementById('recall-project').value,
    query: document.getElementById('recall-query').value,
    limit: Number(document.getElementById('recall-limit').value) || 10,
    filter: document.getElementById('recall-filter').value || null,
  };
  const tbody = document.querySelector('#recall-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="4" class="muted">검색 중…</td></tr>`;
  const r = await fetch('/api/memory/recall', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { tbody.innerHTML = `<tr><td colspan="4" class="err">${r.status}: ${await r.text()}</td></tr>`; return; }
  const d = await r.json();
  tbody.innerHTML = d.hits.length
    ? d.hits.map(h => `<tr><td>${h.id}</td><td><code>${h.kind}</code></td><td>${h.scope}</td><td>${h.body.replace(/</g,'&lt;')}</td></tr>`).join('')
    : `<tr><td colspan="4" class="muted">결과 없음</td></tr>`;
};
document.getElementById('save-form').onsubmit = async (ev) => {
  ev.preventDefault();
  let metadata = {};
  const raw = document.getElementById('save-metadata').value.trim();
  if (raw) { try { metadata = JSON.parse(raw); } catch (e) { document.getElementById('save-result').innerHTML = `<span class="err">JSON 파싱 실패: ${e}</span>`; return; } }
  const body = {
    project: document.getElementById('save-project').value,
    kind: document.getElementById('save-kind').value || 'note',
    body: document.getElementById('save-body').value,
    metadata,
  };
  const r = await fetch('/api/memory/save', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  const out = document.getElementById('save-result');
  if (!r.ok) { out.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  out.textContent = `저장 완료 (id=${d.id})`;
};
document.getElementById('blocks-list-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = document.getElementById('blocks-project').value;
  const tbody = document.querySelector('#blocks-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="2" class="muted">불러오는 중…</td></tr>`;
  const r = await fetch(`/api/memory/blocks?project=${encodeURIComponent(project)}`);
  if (!r.ok) { tbody.innerHTML = `<tr><td colspan="2" class="err">${r.status}: ${await r.text()}</td></tr>`; return; }
  const d = await r.json();
  tbody.innerHTML = d.blocks.length
    ? d.blocks.map(b => `<tr><td><code>${b.kind.replace(/^block:/,'')}</code></td><td>${b.body.replace(/</g,'&lt;')}</td></tr>`).join('')
    : `<tr><td colspan="2" class="muted">블록 없음</td></tr>`;
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
  if (!r.ok) { out.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json(); out.textContent = `저장 완료 (id=${d.id})`;
};
document.getElementById('export-form').onsubmit = (ev) => {
  ev.preventDefault();
  const project = document.getElementById('export-project').value;
  window.location.href = `/api/memory/export?project=${encodeURIComponent(project)}`;
};

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
  sum.textContent = '압축 중…'; pre.style.display = 'none';
  const r = await fetch('/api/compress', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const ratio = d.original_len ? ((d.compressed_len / d.original_len) * 100).toFixed(1) : '0';
  sum.textContent = `mode=${d.mode}${d.scorer ? ` · scorer=${d.scorer}` : ''} · ${d.original_len} → ${d.compressed_len} chars (${ratio}%) · 절감 ${d.saved_chars}`;
  pre.style.display = 'block'; pre.textContent = d.compressed;
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
  sum.textContent = '필터링 중…'; pre.style.display = 'none';
  const r = await fetch('/api/proxy', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const ratio = d.original_len ? ((d.filtered_len / d.original_len) * 100).toFixed(1) : '0';
  sum.textContent = `mode=${d.mode} · ${d.original_len} → ${d.filtered_len} chars (${ratio}%) · 절감 ${d.saved_chars}`;
  pre.style.display = 'block'; pre.textContent = d.filtered;
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
  meta.textContent = '진단 중…'; pre.style.display = 'none';
  const r = await fetch('/api/diagnose', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { meta.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  meta.textContent = `${d.provider}/${d.model} · in ${d.input_tokens} · out ${d.output_tokens}`;
  pre.style.display = 'block'; pre.textContent = d.diagnosis;
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
  sum.textContent = '스캔 중…'; pre.style.display = 'none';
  const r = await fetch('/api/repo-map', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { sum.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  sum.textContent = `${d.files} 파일 · ${d.total_bytes} bytes 스캔 · 시그니처 ${d.signature_chars} chars`;
  pre.style.display = 'block';
  pre.textContent = d.entries.map(e => `// ${e.path} [${e.language || ''}]\n${e.signatures}\n`).join('\n');
};

const CAT_LABEL = { development: '개발', design: '디자인', planning: '설계' };
let LOADED_TEMPLATES = [];
async function loadTemplates() {
  const tpls = await fetch('/api/templates').then(r => r.json()).catch(() => []);
  LOADED_TEMPLATES = tpls;
  const grouped = { development: [], design: [], planning: [] };
  for (const t of tpls) {
    const c = (t.category || 'development');
    (grouped[c] || grouped.development).push(t);
  }
  const rows = [];
  for (const cat of ['development', 'design', 'planning']) {
    for (const t of grouped[cat]) {
      rows.push(`<tr><td><code>${t.name}</code></td><td>${CAT_LABEL[cat]}</td><td>${t.description}</td><td><a data-pick="${t.name}" style="cursor:pointer;color:var(--accent);">사용</a></td></tr>`);
    }
  }
  document.querySelector('#tpl tbody').innerHTML = rows.join('') || '<tr><td colspan="4" class="muted">템플릿 없음</td></tr>';
  document.querySelectorAll('a[data-pick]').forEach(a => a.onclick = () => {
    document.getElementById('scaffold-template').value = a.dataset.pick;
    renderScaffoldVars();
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
  if (!r.ok) { out.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  const rows = d.files.map(f => `<tr><td><code>${f.path}</code></td><td>${f.bytes}</td></tr>`).join('');
  out.innerHTML = `<div>${d.root} 아래 ${d.files.length} 파일</div><table><thead><tr><th>경로</th><th>bytes</th></tr></thead><tbody>${rows}</tbody></table>`;
};
document.getElementById('scaffold-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const out = document.getElementById('scaffold-result');
  out.textContent = '생성 중…';
  const r = await fetch('/api/templates/scaffold', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(buildScaffoldBody())});
  if (!r.ok) { out.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  out.innerHTML = `생성 완료 — <code>${d.root}</code> 아래 ${d.files_written} 파일` +
    (d.post_hooks.length ? `<br>후처리: ${d.post_hooks.map(h => `<code>${h}</code>`).join(', ')}` : '');
};

async function loadPrompts() {
  let prompts = [];
  try { prompts = await fetch('/api/prompts').then(r => r.ok ? r.json() : []); } catch (_) {}
  const tbody = document.querySelector('#prompts-tbl tbody');
  if (!prompts.length) { tbody.innerHTML = `<tr><td colspan="3" class="muted">아직 저장된 프롬프트 없음</td></tr>`; return; }
  tbody.innerHTML = prompts.map(p => {
    const versions = p.versions.map(v => `<a data-name="${p.name}" data-version="${v}" style="margin-right:0.5rem;cursor:pointer;color:var(--accent);">v${v}</a>`).join('');
    return `<tr><td><code>${p.name}</code></td><td>v${p.latest}</td><td>${versions}</td></tr>`;
  }).join('');
  tbody.querySelectorAll('a[data-name]').forEach(a => a.onclick = async () => {
    const body = await fetch(`/api/prompts/${a.dataset.name}/${a.dataset.version}`).then(r => r.json());
    const pre = document.getElementById('prompt-body');
    pre.style.display = 'block';
    pre.textContent = `# ${body.name} v${body.version}\n\n${body.body}`;
  });
}

document.getElementById('setup-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    agent: document.getElementById('setup-agent').value,
    memory: document.getElementById('setup-memory').value || null,
    binary: document.getElementById('setup-binary').value || null,
  };
  const pre = document.getElementById('setup-output');
  pre.style.display = 'none';
  const r = await fetch('/api/setup', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!r.ok) { pre.style.display = 'block'; pre.innerHTML = `<span class="err">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  pre.style.display = 'block';
  pre.textContent = `# ${d.agent} → ${d.target_path}\n\n${d.snippet}`;
};

loadOverview();
loadTemplates();
loadPrompts();
setInterval(loadOverview, 5000);
</script>
</body>
</html>
"#;
