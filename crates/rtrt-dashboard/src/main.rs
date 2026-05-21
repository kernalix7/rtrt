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
    auto_capture: bool,
    default_project: String,
}

impl AppState {
    /// Best-effort auto-save into the memory store. Failures are logged and
    /// swallowed so the calling handler still returns the user-visible result.
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
        let project = self.default_project.clone();
        let body = body.to_string();
        let kind = kind.to_string();
        let metadata = metadata.clone();
        let guard = store.lock().await;
        let result = if metadata.is_empty() {
            guard.save(&project, &kind, &body)
        } else {
            guard.save_with_metadata(&project, &kind, &body, &metadata)
        };
        if let Err(e) = result {
            tracing::warn!("auto-capture {kind}: {e}");
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
    let gateway = Arc::new(Gateway::from_env());
    let prompts = open_prompt_registry();
    let memory = open_memory_store();
    let auto_capture = std::env::var("RTRT_AUTO_CAPTURE")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true);
    let default_project =
        std::env::var("RTRT_DEFAULT_PROJECT").unwrap_or_else(|_| "default".into());
    if auto_capture {
        tracing::info!(
            "auto-capture on (project={default_project}, kinds=chat/compress/diagnose/proxy)"
        );
    } else {
        tracing::info!("auto-capture off (RTRT_AUTO_CAPTURE=0)");
    }
    let state = AppState {
        gateway,
        prompts,
        memory,
        auto_capture,
        default_project,
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
            serde_json::json!({
                "id": r.id,
                "kind": r.kind,
                "scope": r.scope,
                "body": r.body,
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

async fn compress(
    State(state): State<AppState>,
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
    let mut meta = BTreeMap::new();
    meta.insert("source".into(), "compress".into());
    meta.insert("mode".into(), mode.to_string());
    if let Some(s) = scorer.as_deref() {
        meta.insert("scorer".into(), s.to_string());
    }
    meta.insert(
        "saved_chars".into(),
        original.saturating_sub(compressed).to_string(),
    );
    state.capture("compress", &out, &meta).await;
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

  /* Progressive disclosure */
  details > summary.muted-summary { cursor: pointer; color: var(--muted); font-size: 0.85em; padding: 0.35rem 0; list-style: none; user-select: none; }
  details > summary.muted-summary::-webkit-details-marker { display: none; }
  details > summary.muted-summary::before { content: '⇲ '; font-size: 0.9em; }
  details[open] > summary.muted-summary::before { content: '⇱ '; }
  details > .adv { padding-top: 0.5rem; display: grid; gap: 0.5rem; }

  /* Graph canvas */
  #graph-canvas { cursor: grab; }

  /* Memory project picker */
  .proj-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 0.85rem; }
  .proj-card { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 1rem 1.1rem; cursor: pointer; transition: transform .15s ease, box-shadow .15s ease, border-color .15s ease; }
  .proj-card:hover { transform: translateY(-2px); border-color: var(--accent); box-shadow: var(--shadow); }
  .proj-card .name { font-weight: 600; font-size: 1.05rem; }
  .proj-card .row2 { display: flex; justify-content: space-between; color: var(--muted); font-size: 0.85em; margin-top: 0.35rem; }
  .proj-head { display: flex; align-items: baseline; gap: 0.75rem; margin-bottom: 1rem; flex-wrap: wrap; }
  .proj-head h2 { font-size: 1.3rem; }
  .hist-list { display: flex; flex-direction: column; gap: 0.4rem; }
  .hist-item { display: flex; gap: 0.75rem; padding: 0.55rem 0.65rem; background: var(--bg); border: 1px solid var(--border); border-radius: 8px; align-items: baseline; }
  .hist-item .when { color: var(--muted); font-size: 0.82em; min-width: 72px; font-variant-numeric: tabular-nums; }
  .hist-item .kind { color: var(--accent); font-size: 0.78em; min-width: 60px; }
  .hist-item .body { flex: 1; white-space: pre-wrap; overflow-wrap: anywhere; }
  .pager { display: flex; align-items: center; gap: 0.6rem; margin-top: 0.85rem; justify-content: center; }
  .pager button:disabled { opacity: 0.4; cursor: default; }

  /* Template category cards */
  .tpl-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 1rem; }
  .tpl-card { background: var(--surface); border: 1px solid var(--border); border-radius: 14px; padding: 1.5rem 1.25rem; cursor: pointer; transition: transform .15s ease, box-shadow .15s ease, border-color .15s ease; }
  .tpl-card:hover { transform: translateY(-3px); border-color: var(--accent); box-shadow: var(--shadow); }
  .tpl-card .num { color: var(--muted); font-size: 0.78em; letter-spacing: 0.08em; }
  .tpl-card h3 { margin: 0.4rem 0 0.4rem; font-size: 1.35rem; }
  .tpl-card .desc { color: var(--muted); font-size: 0.92em; line-height: 1.5; }
  .tpl-card .go { color: var(--accent); font-weight: 600; margin-top: 0.85rem; display: inline-block; }

  /* Modal */
  .modal-backdrop { position: fixed; inset: 0; background: rgba(15,23,42,.45); display: flex; align-items: center; justify-content: center; z-index: 50; }
  .modal-backdrop[hidden] { display: none; }
  .modal { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 1.25rem 1.5rem; width: min(480px, 90vw); box-shadow: 0 12px 48px rgba(0,0,0,.25); }
  .modal-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 0.75rem; }
  .modal-head h2 { margin: 0; font-size: 1.1rem; }
  .modal-head button { background: transparent; border: 0; cursor: pointer; font-size: 1.1rem; }

  /* Command palette */
  .palette-backdrop { position: fixed; inset: 0; background: rgba(15,23,42,.4); display: none; align-items: flex-start; justify-content: center; padding-top: 10vh; z-index: 100; }
  .palette-backdrop.open { display: flex; }
  .palette { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; box-shadow: 0 12px 48px rgba(0,0,0,.25); width: min(520px, 90vw); overflow: hidden; }
  .palette input { width: 100%; padding: 1rem; border: 0; border-bottom: 1px solid var(--border); border-radius: 0; font-size: 1.05rem; }
  .palette input:focus { box-shadow: none; }
  .palette ul { list-style: none; margin: 0; padding: 0.25rem; max-height: 50vh; overflow-y: auto; }
  .palette li { padding: 0.55rem 0.75rem; border-radius: 6px; cursor: pointer; display: flex; align-items: center; gap: 0.6rem; }
  .palette li.active, .palette li:hover { background: var(--accent-soft); color: var(--accent); }
  .palette li .meta { color: var(--muted); font-size: 0.85em; margin-left: auto; }

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
  <button id="open-palette" title="빠른 이동 (Ctrl/Cmd + K)" style="cursor:pointer;background:transparent;border:1px solid var(--border);color:var(--muted);padding:0.3rem 0.7rem;border-radius:6px;font-size:0.85em;">⌘K 빠른 이동</button>
  <button id="theme-toggle" title="다크/라이트">🌗</button>
</div>

<div class="layout">
  <aside>
    <div class="group">개요</div>
    <a class="nav active" data-page="overview"><span class="icon">🏠</span>대시보드</a>
    <a class="nav" data-page="memory"><span class="icon">🧠</span>메모리</a>
    <div class="group">도구</div>
    <a class="nav" data-page="compress"><span class="icon">🗜</span>압축</a>
    <a class="nav" data-page="diagnose"><span class="icon">🩺</span>진단</a>
    <a class="nav" data-page="repomap"><span class="icon">🗺</span>코드 맵</a>
    <a class="nav" data-page="templates"><span class="icon">📁</span>템플릿</a>
    <a class="nav" data-page="prompts"><span class="icon">📝</span>프롬프트</a>
    <div class="group">설정</div>
    <a class="nav" data-page="connect"><span class="icon">🔌</span>에이전트 연결</a>
    <a class="nav" data-page="env"><span class="icon">⚙️</span>환경</a>
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

      <!-- 프로젝트 선택 -->
      <div id="mem-projects" class="mem-projects">
        <div style="display:flex;justify-content:space-between;align-items:baseline;margin-bottom:0.75rem;">
          <h2 style="margin:0;">프로젝트</h2>
          <a id="mem-new-project" style="cursor:pointer;">+ 새 프로젝트</a>
        </div>
        <div id="mem-project-grid" class="proj-grid"></div>
      </div>

      <!-- 프로젝트 상세 -->
      <div id="mem-detail" hidden>
        <div class="proj-head">
          <a id="mem-back" style="cursor:pointer;">← 프로젝트 목록</a>
          <h2 id="mem-detail-name" style="margin:0;"></h2>
          <span id="mem-detail-meta" class="muted"></span>
        </div>
        <nav class="subtabs" id="memory-subtabs">
          <a class="active" data-sub="memhistory">히스토리</a>
          <a data-sub="memquery">검색</a>
          <a data-sub="memmap">맵</a>
          <a data-sub="memblocks">블록</a>
          <a data-sub="membackup">백업</a>
        </nav>

        <div id="sub-memhistory" class="subpage">
          <div class="card">
            <div class="head"><h2>히스토리</h2><span id="history-meta" class="hint">전부 영구 저장 · 페이지 단위 50건</span></div>
            <div id="history-list" class="hist-list"><div class="empty">불러오는 중…</div></div>
            <div id="history-pager" class="pager" hidden>
              <button id="history-prev" class="ghost" type="button">← 이전</button>
              <span id="history-page" class="muted"></span>
              <button id="history-next" class="ghost" type="button">다음 →</button>
            </div>
            <div style="margin-top:0.75rem;">
              <details><summary class="muted-summary">새 메모리 빠른 저장</summary>
                <div class="adv">
                  <form id="save-form">
                    <input id="save-project" type="hidden">
                    <textarea id="save-body" rows="3" placeholder="저장할 내용" required></textarea>
                    <div style="display:flex;gap:0.5rem;align-items:center;">
                      <button class="primary" type="submit">저장</button>
                    </div>
                    <details><summary class="muted-summary">메타데이터 / kind</summary>
                      <div class="adv">
                        <div class="row">
                          <input id="save-kind" placeholder="kind (기본 note)" value="note">
                          <input id="save-metadata" placeholder='메타: {"source":"claude"}'>
                        </div>
                      </div>
                    </details>
                  </form>
                  <div id="save-result" class="out-meta"></div>
                </div>
              </details>
            </div>
          </div>
        </div>

        <div id="sub-memquery" class="subpage" hidden>
          <div class="card">
            <div class="head"><h2>검색</h2></div>
            <form id="recall-form">
              <input id="recall-project" type="hidden">
              <input id="recall-query" placeholder="검색어" required>
              <button class="primary" type="submit">검색</button>
              <details><summary class="muted-summary">고급 옵션</summary>
                <div class="adv">
                  <div class="row">
                    <input id="recall-limit" type="number" min="1" max="50" value="10" title="최대 결과 수">
                    <input id="recall-filter" placeholder="payload 필터 (예: source=claude)">
                  </div>
                </div>
              </details>
            </form>
            <table id="recall-tbl" style="margin-top:0.75rem;"><thead><tr><th>id</th><th>kind</th><th>내용</th></tr></thead><tbody><tr><td colspan="3" class="empty">검색어를 입력하세요.</td></tr></tbody></table>
          </div>
        </div>

        <div id="sub-memmap" class="subpage" hidden>
          <div class="card">
            <div class="head"><h2>지식 맵</h2><span class="hint">노드 = 메모리, 선 = 연결</span></div>
            <form id="graph-form" style="grid-template-columns:1fr auto;">
              <input id="graph-project" type="hidden">
              <button class="primary" type="submit">맵 보기</button>
            </form>
            <div id="graph-meta" class="out-meta"></div>
            <canvas id="graph-canvas" width="800" height="420" style="display:block;width:100%;height:420px;background:var(--bg);border:1px solid var(--border);border-radius:8px;margin-top:0.5rem;"></canvas>
            <div id="graph-tip" class="out-meta">노드 위에 마우스를 올리면 내용 미리보기.</div>
          </div>
        </div>

        <div id="sub-memblocks" class="subpage" hidden>
          <div class="card">
            <div class="head"><h2>블록</h2><span class="hint">persona / human / context 같은 영속 슬롯</span></div>
            <form id="blocks-list-form" style="grid-template-columns:1fr auto;">
              <input id="blocks-project" type="hidden">
              <button type="submit">목록 새로고침</button>
            </form>
            <table id="blocks-tbl" style="margin-top:0.5rem;"><thead><tr><th>이름</th><th>내용</th></tr></thead><tbody><tr><td colspan="2" class="empty">아직 블록 없음.</td></tr></tbody></table>
            <form id="blocks-set-form" style="margin-top:0.75rem;">
              <input id="block-set-project" type="hidden">
              <input id="block-set-name" placeholder="이름 (persona / human / context)" required>
              <textarea id="block-set-body" rows="2" placeholder="블록 내용" required></textarea>
              <button class="primary" type="submit">저장 / 덮어쓰기</button>
            </form>
            <div id="block-set-result" class="out-meta"></div>
          </div>
        </div>

        <div id="sub-membackup" class="subpage" hidden>
          <div class="card">
            <div class="head"><h2>백업</h2><span class="hint">JSON Lines로 내려받기</span></div>
            <form id="export-form">
              <input id="export-project" type="hidden">
              <button class="primary" type="submit">다운로드</button>
            </form>
          </div>
        </div>
      </div>
    </section>

    <!-- 압축 -->
    <section id="page-compress" class="page" hidden>
      <div class="card">
        <div class="head"><h2>텍스트 압축</h2><span class="hint">기본 full 레벨 룰 엔진</span></div>
        <form id="compress-form">
          <textarea id="compress-input" rows="6" placeholder="줄일 텍스트" required></textarea>
          <div style="display:flex;gap:0.5rem;align-items:center;">
            <button class="primary" type="submit">압축</button>
            <a data-sample="compress" style="cursor:pointer;">예시 채우기</a>
          </div>
          <details><summary class="muted-summary">고급 옵션</summary>
            <div class="adv">
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
                <input id="compress-ratio" type="number" step="0.05" min="0.1" max="1" value="0.5" title="ML ratio">
              </div>
            </div>
          </details>
        </form>
        <div id="compress-summary" class="out-meta"></div>
        <pre id="compress-output" hidden></pre>
      </div>
      <div class="card">
        <div class="head"><h2>명령 출력 필터</h2><span class="hint">git / cargo 노이즈 정리</span></div>
        <form id="proxy-form">
          <input id="proxy-command" placeholder="명령 (예: git status)">
          <textarea id="proxy-raw" rows="6" placeholder="원본 stdout / stderr" required></textarea>
          <div style="display:flex;gap:0.5rem;align-items:center;">
            <button class="primary" type="submit">필터링</button>
            <a data-sample="proxy" style="cursor:pointer;">예시 채우기</a>
          </div>
          <details><summary class="muted-summary">고급 옵션</summary>
            <div class="adv">
              <div class="row">
                <select id="proxy-mode">
                  <option value="command">command 자동</option>
                  <option value="errors_only">errors_only</option>
                  <option value="ultra_compact">ultra_compact</option>
                </select>
                <input id="proxy-context" type="number" min="0" max="20" value="3" title="errors_only 컨텍스트">
              </div>
            </div>
          </details>
        </form>
        <div id="proxy-summary" class="out-meta"></div>
        <pre id="proxy-output" hidden></pre>
      </div>
    </section>

    <!-- 진단 -->
    <section id="page-diagnose" class="page" hidden>
        <div class="card">
          <div class="head"><h2>실패 진단</h2><span class="hint">로그 → LLM이 원인 + 수정 한 줄</span></div>
          <form id="diagnose-form">
            <input id="diagnose-model" placeholder="model (예: claude-haiku-4-5)" required>
            <textarea id="diagnose-raw" rows="8" placeholder="빌드 / 테스트 실패 로그" required></textarea>
            <div style="display:flex;gap:0.5rem;">
              <button class="primary" type="submit">진단 요청</button>
              <button type="button" class="ghost" data-sample="diagnose">예시 채우기</button>
            </div>
            <details><summary class="muted-summary">고급 옵션</summary>
              <div class="adv">
                <input id="diagnose-context" type="number" min="0" max="20" value="3" title="컨텍스트 줄 수">
              </div>
            </details>
          </form>
          <div id="diagnose-meta" class="out-meta"></div>
          <pre id="diagnose-output" hidden></pre>
        </div>
    </section>

    <!-- 코드 맵 -->
    <section id="page-repomap" class="page" hidden>
        <div class="card">
          <div class="head"><h2>코드 시그니처 맵</h2><span class="hint">Rust / Python / TypeScript 자동</span></div>
          <form id="repomap-form">
            <input id="repomap-root" placeholder="프로젝트 경로 (예: /home/user/projects/foo)" required>
            <button class="primary" type="submit">맵 생성</button>
            <details><summary class="muted-summary">고급 옵션</summary>
              <div class="adv">
                <div class="row">
                  <input id="repomap-ext" placeholder="확장자 (비우면 자동)">
                  <input id="repomap-max" type="number" min="1024" step="1024" value="524288" title="파일당 최대 바이트">
                </div>
              </div>
            </details>
          </form>
          <div id="repomap-summary" class="out-meta"></div>
          <pre id="repomap-output" style="max-height:420px;overflow:auto;" hidden></pre>
        </div>
    </section>

    <!-- 템플릿 -->
    <section id="page-templates" class="page" hidden>
      <div id="tpl-grid" class="tpl-grid"></div>
      <div id="scaffold-modal" class="modal-backdrop" hidden>
        <div class="modal">
          <div class="modal-head">
            <h2 id="scaffold-title">새 프로젝트</h2>
            <button id="scaffold-close" class="ghost" aria-label="닫기">✕</button>
          </div>
          <form id="scaffold-form">
            <input id="scaffold-template" type="hidden">
            <input id="scaffold-target" placeholder="대상 디렉터리 (예: ./my-project)" required>
            <div id="scaffold-vars"></div>
            <label><input id="scaffold-overwrite" type="checkbox"> 기존 파일 덮어쓰기</label>
            <div style="display:flex;gap:0.5rem;align-items:center;">
              <button type="submit" class="primary">생성</button>
              <a id="scaffold-preview" style="cursor:pointer;">미리보기</a>
            </div>
          </form>
          <div id="scaffold-result" class="out-meta"></div>
        </div>
      </div>
    </section>

    <!-- 프롬프트 -->
    <section id="page-prompts" class="page" hidden>
        <div class="card">
          <div class="head"><h2>버전 프롬프트</h2><span class="hint">$RTRT_PROMPTS_DIR (기본 ~/.rtrt/prompts)</span></div>
          <table id="prompts-tbl"><thead><tr><th>이름</th><th>최신</th><th>버전</th></tr></thead><tbody><tr><td colspan="3" class="empty">아직 저장된 프롬프트 없음. CLI <code>rtrt prompt save</code> 로 추가하세요.</td></tr></tbody></table>
          <pre id="prompt-body" hidden></pre>
        </div>
    </section>

    <!-- 에이전트 연결 -->
    <section id="page-connect" class="page" hidden>
        <div class="card">
          <div class="head"><h2>MCP 설정 스니펫</h2><span class="hint">Claude Code / Cursor / Codex · 디스크 미기록</span></div>
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
    </section>

    <!-- 환경 -->
    <section id="page-env" class="page" hidden>
        <div class="card">
          <div class="head"><h2>환경 정보</h2><span class="hint">현재 바인딩 / 인증 / 캐시 / 예산</span></div>
          <table id="env-tbl"><tbody>
            <tr><td>대시보드 바인드</td><td><code id="env-bind">—</code></td></tr>
            <tr><td>인증 토큰</td><td id="env-token">—</td></tr>
            <tr><td>응답 캐시</td><td id="env-cache">—</td></tr>
            <tr><td>예산 한도</td><td id="env-budget">—</td></tr>
          </tbody></table>
        </div>
    </section>
  </main>
</div>

<div id="palette-backdrop" class="palette-backdrop" role="dialog" aria-hidden="true">
  <div class="palette">
    <input id="palette-input" placeholder="이동할 곳 또는 작업 (예: 메모리, 압축, 진단)" autocomplete="off">
    <ul id="palette-list"></ul>
  </div>
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
document.querySelectorAll('aside a.nav').forEach(a => a.onclick = () => navigate(a.dataset.page));

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
wireSubtabs('memory-subtabs');

// Memory: project picker + drill-in
function relativeTime(ts) {
  if (!ts) return '—';
  const diff = Math.floor(Date.now() / 1000 - ts);
  if (diff < 60) return '방금 전';
  if (diff < 3600) return `${Math.floor(diff / 60)}분 전`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}시간 전`;
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)}일 전`;
  const d = new Date(ts * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}
async function loadProjects() {
  const r = await fetch('/api/memory/projects');
  if (!r.ok) return;
  const d = await r.json();
  const grid = document.getElementById('mem-project-grid');
  if (!d.projects.length) {
    grid.innerHTML = `<div class="empty" style="grid-column:1/-1;">저장된 메모리가 없습니다. 우측 상단 <a id="mem-empty-new" style="cursor:pointer;">+ 새 프로젝트</a> 또는 CLI <code>rtrt memory save</code> 로 시작하세요.</div>`;
    const e = document.getElementById('mem-empty-new');
    if (e) e.onclick = () => promptNewProject();
    return;
  }
  grid.innerHTML = d.projects.map(p =>
    `<div class="proj-card" data-pick="${p.project}">
       <div class="name">${p.project}</div>
       <div class="row2"><span>${p.count}건</span><span>${relativeTime(p.latest_ts)}</span></div>
     </div>`
  ).join('');
  grid.querySelectorAll('.proj-card').forEach(c => c.onclick = () => openProject(c.dataset.pick));
}
function promptNewProject() {
  const name = prompt('새 프로젝트 이름 (예: rtrt)');
  if (name && name.trim()) openProject(name.trim());
}
document.getElementById('mem-new-project').onclick = promptNewProject;
document.getElementById('mem-back').onclick = () => {
  document.getElementById('mem-detail').hidden = true;
  document.getElementById('mem-projects').hidden = false;
  loadProjects();
};

let CURRENT_PROJECT = null;
function openProject(name) {
  CURRENT_PROJECT = name;
  document.getElementById('mem-projects').hidden = true;
  const detail = document.getElementById('mem-detail');
  detail.hidden = false;
  document.getElementById('mem-detail-name').textContent = name;
  // Auto-fill every project input on the detail panes.
  ['recall-project', 'save-project', 'blocks-project', 'block-set-project', 'export-project', 'graph-project'].forEach(id => {
    const el = document.getElementById(id);
    if (el) el.value = name;
  });
  localStorage.setItem('rtrt-project', name);
  // Default sub = history.
  document.querySelectorAll('#memory-subtabs a').forEach(x => x.classList.remove('active'));
  document.querySelector('#memory-subtabs a[data-sub="memhistory"]').classList.add('active');
  document.querySelectorAll('#mem-detail .subpage').forEach(x => x.hidden = true);
  document.getElementById('sub-memhistory').hidden = false;
  loadHistory(name);
}
const HISTORY_PAGE_SIZE = 50;
let HISTORY_OFFSET = 0;
let HISTORY_TOTAL = 0;
async function loadHistory(name, offset) {
  if (offset === undefined) offset = 0;
  HISTORY_OFFSET = offset;
  const list = document.getElementById('history-list');
  list.innerHTML = '<div class="empty">불러오는 중…</div>';
  const r = await fetch(`/api/memory/timeline?project=${encodeURIComponent(name)}&limit=${HISTORY_PAGE_SIZE}&offset=${offset}`);
  if (!r.ok) { list.innerHTML = `<div class="empty err">${r.status}: ${await r.text()}</div>`; return; }
  const d = await r.json();
  HISTORY_TOTAL = d.total || 0;
  if (!d.items.length) {
    list.innerHTML = `<div class="empty">아직 메모리 없음. 아래 '새 메모리 빠른 저장' 으로 시작하세요.</div>`;
    document.getElementById('history-pager').hidden = true;
    document.getElementById('history-meta').textContent = '전부 영구 저장 · 페이지 단위 50건';
    return;
  }
  list.innerHTML = d.items.map(i =>
    `<div class="hist-item">
       <span class="when">${relativeTime(i.created_at)}</span>
       <span class="kind">${i.kind}</span>
       <span class="body">${i.body.replace(/</g, '&lt;')}</span>
     </div>`
  ).join('');
  const pager = document.getElementById('history-pager');
  const totalPages = Math.max(1, Math.ceil(HISTORY_TOTAL / HISTORY_PAGE_SIZE));
  const currentPage = Math.floor(offset / HISTORY_PAGE_SIZE) + 1;
  document.getElementById('history-page').textContent = `${currentPage} / ${totalPages} · 총 ${HISTORY_TOTAL}건`;
  document.getElementById('history-prev').disabled = currentPage <= 1;
  document.getElementById('history-next').disabled = currentPage >= totalPages;
  pager.hidden = totalPages <= 1;
  document.getElementById('history-meta').textContent = `${HISTORY_TOTAL}건 전체 영구 저장 · 페이지당 ${HISTORY_PAGE_SIZE}건`;
}
document.getElementById('history-prev').onclick = () => {
  if (!CURRENT_PROJECT) return;
  loadHistory(CURRENT_PROJECT, Math.max(0, HISTORY_OFFSET - HISTORY_PAGE_SIZE));
};
document.getElementById('history-next').onclick = () => {
  if (!CURRENT_PROJECT) return;
  loadHistory(CURRENT_PROJECT, HISTORY_OFFSET + HISTORY_PAGE_SIZE);
};
// Re-load history after save.
const originalSaveHandler = () => {};

// Project name memory — last-used value persists across reloads so the user
// doesn't have to retype it every time.
const PROJECT_INPUTS = ['recall-project','save-project','blocks-project','block-set-project','export-project','graph-project'];
function syncProject(value) {
  if (!value) return;
  localStorage.setItem('rtrt-project', value);
  PROJECT_INPUTS.forEach(id => { const el = document.getElementById(id); if (el && !el.value) el.value = value; });
}
const savedProject = localStorage.getItem('rtrt-project');
if (savedProject) {
  PROJECT_INPUTS.forEach(id => { const el = document.getElementById(id); if (el) el.value = savedProject; });
}
PROJECT_INPUTS.forEach(id => {
  const el = document.getElementById(id);
  if (el) el.addEventListener('change', () => syncProject(el.value));
});

// Sample data — one-click form fillers so the user can try a tool without
// digging up an example. Each entry is a function so it can vary per call.
const SAMPLES = {
  compress() {
    const ta = document.getElementById('compress-input');
    ta.value = '솔직히 말하자면, 사실 이 버그는 정말 기본적으로 파서에서 발생합니다. 제가 생각하기에 우리가 해야 할 일은, 사실 입력 검증을 좀 더 추가하는 것입니다. 다시 말해, 모든 사용자 입력에 대해 기본적으로 sanitize를 적용해야 합니다.';
    pushActivity('압축 예시 채움');
  },
  proxy() {
    document.getElementById('proxy-command').value = 'cargo build';
    const ta = document.getElementById('proxy-raw');
    ta.value = '   Compiling rtrt-core v0.1.0\n   Compiling rtrt-compress v0.1.0\n   Compiling rtrt-memory v0.1.0\nerror[E0599]: no method named `foo` found for struct `Bar`\n   --> crates/rtrt-memory/src/lib.rs:204:18\n    |\n204 |         store.foo();\n    |               ^^^ method not found\n   Compiling rtrt-providers v0.1.0\nwarning: unused variable `x`\n   --> src/lib.rs:42:9\nerror: could not compile `rtrt-memory` due to previous error';
    pushActivity('필터 예시 채움');
  },
  diagnose() {
    document.getElementById('diagnose-model').value = 'claude-haiku-4-5';
    const ta = document.getElementById('diagnose-raw');
    ta.value = 'test result: FAILED. 1 passed; 1 failed; 0 ignored\n\nfailures:\n---- tests::roundtrip stdout ----\nthread \'tests::roundtrip\' panicked at \'assertion `left == right` failed\n  left: "hello"\n right: "Hello"\', src/lib.rs:42:9\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace';
    pushActivity('진단 예시 채움');
  },
};
document.querySelectorAll('[data-sample]').forEach(btn => btn.onclick = () => {
  const fn = SAMPLES[btn.dataset.sample];
  if (fn) fn();
});

// Command palette — Cmd+K / Ctrl+K opens. Searches pages + sub-tabs + samples.
const PALETTE_ITEMS = [
  { label: '대시보드', hint: 'overview', run: () => navigate('overview') },
  { label: '메모리 — 프로젝트', hint: 'memory · projects', run: () => navigate('memory') },
  { label: '압축', hint: 'compress / proxy', run: () => { navigate('compress'); document.getElementById('compress-input').focus(); } },
  { label: '진단', hint: 'diagnose', run: () => navigate('diagnose') },
  { label: '코드 맵', hint: 'repo-map', run: () => navigate('repomap') },
  { label: '템플릿', hint: 'project scaffolds', run: () => navigate('templates') },
  { label: '프롬프트', hint: 'prompts', run: () => navigate('prompts') },
  { label: '에이전트 연결', hint: 'mcp setup', run: () => navigate('connect') },
  { label: '환경', hint: 'env info', run: () => navigate('env') },
  { label: '테마 토글', hint: 'dark / light', run: () => document.getElementById('theme-toggle').click() },
  { label: '예시: 압축', hint: 'sample · compress', run: () => { navigate('compress'); SAMPLES.compress(); } },
  { label: '예시: 필터', hint: 'sample · proxy', run: () => { navigate('compress'); SAMPLES.proxy(); } },
  { label: '예시: 진단', hint: 'sample · diagnose', run: () => { navigate('diagnose'); SAMPLES.diagnose(); } },
];
function navigate(page) {
  document.querySelectorAll('aside a.nav').forEach(x => x.classList.remove('active'));
  document.querySelectorAll('.page').forEach(x => x.hidden = true);
  const link = document.querySelector(`aside a.nav[data-page="${page}"]`);
  if (link) link.classList.add('active');
  const target = document.getElementById('page-' + page);
  if (target) target.hidden = false;
  if (page === 'memory') {
    // Show project picker by default; user can drill in.
    document.getElementById('mem-detail').hidden = true;
    document.getElementById('mem-projects').hidden = false;
    loadProjects();
  }
}
function subClick(navId, sub) {
  const a = document.querySelector(`#${navId} a[data-sub="${sub}"]`);
  if (a) a.click();
}

const palette = document.getElementById('palette-backdrop');
const paletteInput = document.getElementById('palette-input');
const paletteList = document.getElementById('palette-list');
let paletteIdx = 0;
function renderPalette() {
  const q = paletteInput.value.trim().toLowerCase();
  const items = PALETTE_ITEMS.filter(it => !q || it.label.toLowerCase().includes(q) || it.hint.toLowerCase().includes(q));
  paletteList.innerHTML = items.map((it, i) =>
    `<li data-idx="${i}" class="${i === paletteIdx ? 'active' : ''}">${it.label}<span class="meta">${it.hint}</span></li>`
  ).join('') || '<li class="meta" style="padding:0.75rem;">결과 없음</li>';
  paletteList.dataset.items = JSON.stringify(items.map((_, i) => i));
  paletteList.querySelectorAll('li[data-idx]').forEach(li => li.onclick = () => {
    const idx = Number(li.dataset.idx);
    if (items[idx]) { items[idx].run(); closePalette(); }
  });
}
function openPalette() {
  palette.classList.add('open');
  palette.setAttribute('aria-hidden', 'false');
  paletteInput.value = '';
  paletteIdx = 0;
  renderPalette();
  paletteInput.focus();
}
function closePalette() {
  palette.classList.remove('open');
  palette.setAttribute('aria-hidden', 'true');
}
paletteInput.addEventListener('input', () => { paletteIdx = 0; renderPalette(); });
paletteInput.addEventListener('keydown', (ev) => {
  const items = JSON.parse(paletteList.dataset.items || '[]');
  if (ev.key === 'ArrowDown') { ev.preventDefault(); paletteIdx = (paletteIdx + 1) % Math.max(1, items.length); renderPalette(); }
  else if (ev.key === 'ArrowUp') { ev.preventDefault(); paletteIdx = (paletteIdx - 1 + items.length) % Math.max(1, items.length); renderPalette(); }
  else if (ev.key === 'Enter') {
    const q = paletteInput.value.trim().toLowerCase();
    const filtered = PALETTE_ITEMS.filter(it => !q || it.label.toLowerCase().includes(q) || it.hint.toLowerCase().includes(q));
    if (filtered[paletteIdx]) { filtered[paletteIdx].run(); closePalette(); }
  } else if (ev.key === 'Escape') { closePalette(); }
});
palette.addEventListener('click', (ev) => { if (ev.target === palette) closePalette(); });
document.addEventListener('keydown', (ev) => {
  if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === 'k') {
    ev.preventDefault();
    palette.classList.contains('open') ? closePalette() : openPalette();
  }
});

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

// Memory graph — force-directed canvas layout
let graphState = null;
function initGraph(data) {
  const canvas = document.getElementById('graph-canvas');
  const ctx = canvas.getContext('2d');
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth; const h = canvas.clientHeight;
  canvas.width = w * dpr; canvas.height = h * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

  const accent = getComputedStyle(document.documentElement).getPropertyValue('--accent').trim() || '#2962FF';
  const muted = getComputedStyle(document.documentElement).getPropertyValue('--muted').trim() || '#666';
  const fg = getComputedStyle(document.documentElement).getPropertyValue('--fg').trim() || '#000';

  // Initialise nodes with random positions; edges reference node ids.
  const idIdx = new Map();
  const nodes = data.nodes.map((n, i) => {
    idIdx.set(n.id, i);
    return { ...n, x: w/2 + (Math.random()-0.5)*w*0.6, y: h/2 + (Math.random()-0.5)*h*0.6, vx: 0, vy: 0 };
  });
  const edges = data.edges
    .map(e => ({ s: idIdx.get(e.src), d: idIdx.get(e.dst), rel: e.relation }))
    .filter(e => e.s !== undefined && e.d !== undefined);

  let hover = -1;
  let drag = -1;
  let mouseX = 0, mouseY = 0;
  function getXY(ev) {
    const rect = canvas.getBoundingClientRect();
    return [ev.clientX - rect.left, ev.clientY - rect.top];
  }
  function pick(x, y) {
    for (let i = nodes.length-1; i >= 0; i--) {
      const n = nodes[i];
      const dx = x - n.x; const dy = y - n.y;
      if (dx*dx + dy*dy < 100) return i;
    }
    return -1;
  }
  canvas.onmousemove = (ev) => {
    const [x, y] = getXY(ev);
    mouseX = x; mouseY = y;
    if (drag >= 0) { nodes[drag].x = x; nodes[drag].y = y; nodes[drag].vx = 0; nodes[drag].vy = 0; }
    const h2 = pick(x, y);
    if (h2 !== hover) {
      hover = h2;
      const tip = document.getElementById('graph-tip');
      if (hover >= 0) {
        const n = nodes[hover];
        tip.innerHTML = `<code>#${n.id}</code> ${n.kind} — ${(n.preview || '').replace(/</g,'&lt;')}`;
      } else {
        tip.textContent = '노드 위에 마우스를 올리면 내용 미리보기.';
      }
    }
  };
  canvas.onmousedown = (ev) => { const [x, y] = getXY(ev); drag = pick(x, y); canvas.style.cursor = drag >= 0 ? 'grabbing' : 'grab'; };
  canvas.onmouseup = () => { drag = -1; canvas.style.cursor = 'grab'; };
  canvas.onmouseleave = () => { drag = -1; hover = -1; };

  function step() {
    // Spring-electric layout: edges pull, nodes repel.
    const w = canvas.clientWidth; const h = canvas.clientHeight;
    for (let i = 0; i < nodes.length; i++) {
      const a = nodes[i];
      for (let j = i+1; j < nodes.length; j++) {
        const b = nodes[j];
        const dx = b.x - a.x; const dy = b.y - a.y;
        const d2 = dx*dx + dy*dy + 0.01;
        const f = 1200 / d2;
        const fx = (dx / Math.sqrt(d2)) * f;
        const fy = (dy / Math.sqrt(d2)) * f;
        a.vx -= fx; a.vy -= fy;
        b.vx += fx; b.vy += fy;
      }
    }
    for (const e of edges) {
      const a = nodes[e.s]; const b = nodes[e.d];
      const dx = b.x - a.x; const dy = b.y - a.y;
      const d = Math.sqrt(dx*dx + dy*dy) + 0.01;
      const f = (d - 100) * 0.02;
      const fx = (dx/d) * f; const fy = (dy/d) * f;
      a.vx += fx; a.vy += fy; b.vx -= fx; b.vy -= fy;
    }
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i];
      if (i === drag) continue;
      n.vx *= 0.82; n.vy *= 0.82;
      n.x += n.vx * 0.5; n.y += n.vy * 0.5;
      // Centre attractor + bounds.
      n.vx += (w/2 - n.x) * 0.0008;
      n.vy += (h/2 - n.y) * 0.0008;
      n.x = Math.max(12, Math.min(w-12, n.x));
      n.y = Math.max(12, Math.min(h-12, n.y));
    }

    ctx.clearRect(0, 0, w, h);
    // Edges.
    ctx.strokeStyle = muted; ctx.lineWidth = 1; ctx.globalAlpha = 0.5;
    for (const e of edges) {
      const a = nodes[e.s]; const b = nodes[e.d];
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke();
    }
    ctx.globalAlpha = 1;
    // Nodes.
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i];
      const isBlock = (n.kind || '').startsWith('block:');
      const r = isBlock ? 8 : 6;
      ctx.beginPath(); ctx.arc(n.x, n.y, r, 0, Math.PI * 2);
      ctx.fillStyle = isBlock ? '#7c3aed' : accent;
      ctx.fill();
      if (i === hover) {
        ctx.strokeStyle = fg; ctx.lineWidth = 2; ctx.stroke();
      }
    }
  }
  if (graphState && graphState.raf) cancelAnimationFrame(graphState.raf);
  graphState = { canvas, nodes, edges };
  function loop() { step(); graphState.raf = requestAnimationFrame(loop); }
  loop();
}
document.getElementById('graph-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = document.getElementById('graph-project').value;
  const meta = document.getElementById('graph-meta');
  meta.textContent = '불러오는 중…';
  const r = await fetch(`/api/memory/graph?project=${encodeURIComponent(project)}`);
  if (!r.ok) { meta.innerHTML = `<span style="color:var(--err);">${r.status}: ${await r.text()}</span>`; return; }
  const d = await r.json();
  meta.innerHTML = `<span class="badge ok">${d.nodes.length} 노드</span> <span class="badge">${d.edges.length} 연결</span>`;
  if (!d.nodes.length) { document.getElementById('graph-tip').textContent = '아직 메모리 없음.'; return; }
  initGraph(d);
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
const CAT_NUM = { development: '01', design: '02', planning: '03' };
async function loadTemplates() {
  const tpls = await fetch('/api/templates').then(r => r.ok ? r.json() : []).catch(() => []);
  LOADED_TEMPLATES = tpls;
  const grid = document.getElementById('tpl-grid');
  if (!grid) return;
  const order = ['development', 'design', 'planning'];
  const byCat = {};
  for (const t of tpls) { (byCat[t.category || 'development'] ||= []).push(t); }
  grid.innerHTML = order.map(cat => {
    const list = byCat[cat] || [];
    if (!list.length) return '';
    const t = list[0];
    return `<div class="tpl-card" data-pick="${t.name}">
      <div class="num">${CAT_NUM[cat]} · ${CAT_LABEL[cat].toUpperCase()}</div>
      <h3>${CAT_LABEL[cat]}</h3>
      <div class="desc">${t.description}</div>
      <div class="go">시작 →</div>
    </div>`;
  }).join('');
  grid.querySelectorAll('.tpl-card').forEach(card => card.onclick = () => openScaffoldModal(card.dataset.pick));
}
function openScaffoldModal(name) {
  const tpl = LOADED_TEMPLATES.find(t => t.name === name);
  if (!tpl) return;
  document.getElementById('scaffold-title').textContent = `새 ${CAT_LABEL[tpl.category]} 프로젝트`;
  document.getElementById('scaffold-template').value = name;
  document.getElementById('scaffold-target').value = '';
  document.getElementById('scaffold-overwrite').checked = false;
  document.getElementById('scaffold-result').textContent = '';
  renderScaffoldVars();
  document.getElementById('scaffold-modal').hidden = false;
  setTimeout(() => document.getElementById('scaffold-target').focus(), 0);
}
function closeScaffoldModal() { document.getElementById('scaffold-modal').hidden = true; }
document.getElementById('scaffold-close').onclick = closeScaffoldModal;
document.getElementById('scaffold-modal').onclick = (ev) => { if (ev.target.id === 'scaffold-modal') closeScaffoldModal(); };
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
document.getElementById('open-palette').onclick = openPalette;
pushActivity('대시보드 부팅 완료. ⌘K 또는 Ctrl+K 로 빠르게 이동하세요.');
</script>
</body>
</html>
"#;
