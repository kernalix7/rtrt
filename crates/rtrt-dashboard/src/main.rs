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
        std::env::var("RTRT_DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:3111".to_string());
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
        .route("/api/chat", post(chat))
        .route("/api/metrics", get(metrics))
        .route("/api/prompts", get(list_prompts))
        .route("/api/prompts/{name}", get(list_prompt_versions))
        .route("/api/prompts/{name}/{version}", get(get_prompt))
        .route("/api/budget", get(budget))
        .route("/api/memory/recall", post(memory_recall))
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

    let listener = tokio::net::TcpListener::bind(&bind).await?;
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
    variables: Vec<rtrt_templates::TemplateVariable>,
}

impl From<rtrt_templates::Template> for TemplateSummary {
    fn from(t: rtrt_templates::Template) -> Self {
        Self {
            name: t.name,
            description: t.description,
            source: t.source,
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
<title>RTRT Dashboard</title>
<style>
  :root {
    --bg: #ffffff;
    --fg: #1a1a1a;
    --muted: #666666;
    --border: #eeeeee;
    --code-bg: #f3f3f3;
    --pre-bg: #fafafa;
    --accent: #2962FF;
    --err: #c0392b;
  }
  :root[data-theme="dark"] {
    --bg: #15171a;
    --fg: #e6e6e6;
    --muted: #9aa0a6;
    --border: #2a2d31;
    --code-bg: #23272e;
    --pre-bg: #1c1f23;
    --accent: #6aa3ff;
    --err: #ff6b6b;
  }
  body { font: 14px/1.45 system-ui, sans-serif; max-width: 960px; margin: 2rem auto; padding: 0 1rem; background: var(--bg); color: var(--fg); }
  h1 { margin-bottom: 0.25rem; }
  .sub { color: var(--muted); margin-bottom: 1.5rem; }
  nav { margin-bottom: 1.5rem; display: flex; align-items: center; flex-wrap: wrap; gap: 1rem; }
  nav a { cursor: pointer; color: var(--accent); text-decoration: none; }
  nav a.active { font-weight: 600; text-decoration: underline; }
  nav .spacer { flex: 1; }
  #theme-toggle { cursor: pointer; background: transparent; border: 1px solid var(--border); color: var(--fg); padding: 0.25rem 0.6rem; border-radius: 4px; font-size: 0.9em; }
  section { display: none; margin-bottom: 2rem; }
  section.active { display: block; }
  table { width: 100%; border-collapse: collapse; }
  th, td { padding: 0.4rem 0.6rem; border-bottom: 1px solid var(--border); text-align: left; vertical-align: top; }
  code { background: var(--code-bg); padding: 0 0.25rem; border-radius: 3px; font-family: ui-monospace, monospace; }
  pre { background: var(--pre-bg); border: 1px solid var(--border); }
  input, textarea, select, button { background: var(--bg); color: var(--fg); border: 1px solid var(--border); padding: 0.3rem 0.5rem; border-radius: 3px; font: inherit; }
  button { cursor: pointer; }
  .kpi { display: inline-block; margin-right: 1.5rem; }
  .kpi b { font-size: 1.4rem; }
  .err { color: var(--err); }
</style>
</head>
<body>
<h1>RTRT</h1>
<div class="sub">Rust-based Token Reduction Toolkit</div>

<nav>
  <a data-tab="metrics" class="active">Metrics</a>
  <a data-tab="budget">Budget</a>
  <a data-tab="prompts">Prompts</a>
  <a data-tab="memory">Memory</a>
  <a data-tab="templates">Templates</a>
  <a data-tab="stats">Compression</a>
  <a data-tab="proxy">Proxy</a>
  <a data-tab="diagnose">Diagnose</a>
  <a data-tab="repomap">RepoMap</a>
  <a data-tab="setup">Setup</a>
  <span class="spacer"></span>
  <button id="theme-toggle" title="Toggle dark / light mode">🌗</button>
</nav>

<section id="metrics" class="active">
  <h2>Gateway metrics</h2>
  <div id="metrics-summary">loading…</div>
  <div style="display:grid;grid-template-columns:1fr 1fr;gap:1rem;margin-top:1rem;">
    <div>
      <h3 style="margin-bottom:0.3rem;">Latency (ms)</h3>
      <svg id="chart-latency" width="100%" height="120" viewBox="0 0 400 120" preserveAspectRatio="none" style="border:1px solid var(--border);border-radius:4px;"></svg>
    </div>
    <div>
      <h3 style="margin-bottom:0.3rem;">Tokens (in + out)</h3>
      <svg id="chart-tokens" width="100%" height="120" viewBox="0 0 400 120" preserveAspectRatio="none" style="border:1px solid var(--border);border-radius:4px;"></svg>
    </div>
  </div>
  <h3>Recent requests</h3>
  <table id="metrics-recent"><thead>
    <tr><th>Time</th><th>Provider</th><th>Model</th><th>in</th><th>out</th><th>latency</th><th>status</th></tr>
  </thead><tbody></tbody></table>
</section>

<section id="budget">
  <h2>Spending budget</h2>
  <div id="budget-summary">loading…</div>
  <p style="color:#666;margin-top:1rem;">Attach a cap via <code>Gateway::with_budget</code>. Spend is priced from the recorded metrics window.</p>
</section>

<section id="prompts">
  <h2>Versioned prompts</h2>
  <p style="color:#666;">Backed by <code>PromptRegistry</code> at <code>$RTRT_PROMPTS_DIR</code> (defaults to <code>~/.rtrt/prompts</code>).</p>
  <table id="prompts-tbl"><thead><tr><th>Name</th><th>Latest</th><th>Versions</th></tr></thead><tbody></tbody></table>
  <pre id="prompt-body" style="white-space:pre-wrap;background:#fafafa;border:1px solid #eee;padding:0.75rem;border-radius:4px;display:none;"></pre>
</section>

<section id="memory">
  <h2>Memory recall</h2>
  <p style="color:#666;">BM25 over <code>$RTRT_MEMORY_PATH</code> (defaults to <code>.rtrt/memory.sqlite</code>). qdrant-style payload filter supported.</p>
  <form id="recall-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:600px;">
    <input id="recall-project" placeholder="project" required>
    <input id="recall-limit" type="number" min="1" max="50" value="10">
    <input id="recall-query" placeholder="query" required style="grid-column:1 / span 2;">
    <input id="recall-filter" placeholder="filter (e.g. source=claude,topic~^auth)" style="grid-column:1 / span 2;">
    <button type="submit" style="grid-column:1 / span 2;">Recall</button>
  </form>
  <table id="recall-tbl" style="margin-top:1rem;"><thead><tr><th>id</th><th>kind</th><th>scope</th><th>body</th></tr></thead><tbody></tbody></table>

  <h3 style="margin-top:2rem;">Letta blocks</h3>
  <form id="blocks-list-form" style="display:flex;gap:0.5rem;max-width:600px;align-items:center;">
    <input id="blocks-project" placeholder="project" required>
    <button type="submit">List</button>
  </form>
  <table id="blocks-tbl" style="margin-top:0.5rem;"><thead><tr><th>name</th><th>body</th></tr></thead><tbody></tbody></table>
  <form id="blocks-set-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:600px;margin-top:1rem;">
    <input id="block-set-name" placeholder="name (persona / human / context)" required>
    <input id="block-set-project" placeholder="project" required>
    <textarea id="block-set-body" placeholder="body" required rows="3" style="grid-column:1 / span 2;"></textarea>
    <button type="submit" style="grid-column:1 / span 2;">Set block</button>
  </form>
  <div id="block-set-result" style="margin-top:0.5rem;color:#666;"></div>

  <h3 style="margin-top:2rem;">Save memory</h3>
  <form id="save-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:600px;">
    <input id="save-project" placeholder="project" required>
    <input id="save-kind" placeholder="kind (default: note)" value="note">
    <textarea id="save-body" placeholder="body" required rows="3" style="grid-column:1 / span 2;"></textarea>
    <input id="save-metadata" placeholder="metadata JSON (e.g. {\"source\":\"claude\"})" style="grid-column:1 / span 2;">
    <button type="submit" style="grid-column:1 / span 2;">Save</button>
  </form>
  <div id="save-result" style="margin-top:0.5rem;color:#666;"></div>
</section>

<section id="templates">
  <h2>Project templates</h2>
  <table id="tpl"><thead><tr><th>Name</th><th>Source</th><th>Description</th><th></th></tr></thead><tbody></tbody></table>

  <h3 style="margin-top:2rem;">Scaffold</h3>
  <form id="scaffold-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:720px;">
    <select id="scaffold-template" required></select>
    <input id="scaffold-target" placeholder="target directory" required>
    <div id="scaffold-vars" style="grid-column:1 / span 2;"></div>
    <label style="grid-column:1 / span 2;"><input id="scaffold-overwrite" type="checkbox"> overwrite existing files</label>
    <button type="submit" style="grid-column:1 / span 2;">Scaffold</button>
  </form>
  <div id="scaffold-result" style="margin-top:0.5rem;color:#666;"></div>
</section>

<section id="proxy">
  <h2>Proxy filter</h2>
  <p style="color:var(--muted);">Run rtrt-proxy filters against captured stdout/stderr.</p>
  <form id="proxy-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:720px;">
    <select id="proxy-mode">
      <option value="command">command (auto-detect by label)</option>
      <option value="errors_only">errors_only</option>
      <option value="ultra_compact">ultra_compact</option>
    </select>
    <input id="proxy-command" placeholder="command (e.g. git status)">
    <input id="proxy-context" type="number" min="0" max="20" value="3" title="errors_only context lines">
    <textarea id="proxy-raw" rows="8" placeholder="paste raw output" required style="grid-column:1 / span 2;"></textarea>
    <button type="submit" style="grid-column:1 / span 2;">Filter</button>
  </form>
  <div id="proxy-summary" style="margin-top:0.75rem;color:var(--muted);"></div>
  <pre id="proxy-output" style="white-space:pre-wrap;display:none;padding:0.75rem;border-radius:4px;"></pre>
</section>

<section id="diagnose">
  <h2>Diagnose</h2>
  <p style="color:var(--muted);">Pipe build/test failure to a provider for one-shot triage. Uses gateway routing.</p>
  <form id="diagnose-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:720px;">
    <input id="diagnose-model" placeholder="model (e.g. claude-haiku-4-5)" required>
    <input id="diagnose-context" type="number" min="0" max="20" value="3" title="errors_only context lines">
    <textarea id="diagnose-raw" rows="8" placeholder="paste failing build/test output" required style="grid-column:1 / span 2;"></textarea>
    <button type="submit" style="grid-column:1 / span 2;">Diagnose</button>
  </form>
  <div id="diagnose-meta" style="margin-top:0.75rem;color:var(--muted);"></div>
  <pre id="diagnose-output" style="white-space:pre-wrap;display:none;padding:0.75rem;border-radius:4px;"></pre>
</section>

<section id="repomap">
  <h2>RepoMap</h2>
  <p style="color:var(--muted);">tree-sitter signature index of a Rust project — bodies stripped.</p>
  <form id="repomap-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:720px;">
    <input id="repomap-root" placeholder="root path" required>
    <input id="repomap-ext" placeholder="ext (default .rs)" value=".rs">
    <input id="repomap-max" type="number" min="1024" step="1024" value="524288" title="max bytes per file">
    <button type="submit" style="grid-column:1 / span 2;">Map</button>
  </form>
  <div id="repomap-summary" style="margin-top:0.75rem;color:var(--muted);"></div>
  <pre id="repomap-output" style="white-space:pre-wrap;display:none;padding:0.75rem;border-radius:4px;max-height:480px;overflow:auto;"></pre>
</section>

<section id="setup">
  <h2>Agent setup</h2>
  <p style="color:var(--muted);">Generate an MCP config snippet for popular coding agents (dry-run only — never writes).</p>
  <form id="setup-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:720px;">
    <select id="setup-agent">
      <option value="claude-code">claude-code</option>
      <option value="cursor">cursor</option>
      <option value="codex">codex</option>
    </select>
    <input id="setup-memory" placeholder="memory path (default .rtrt/memory.sqlite)" value=".rtrt/memory.sqlite">
    <input id="setup-binary" placeholder="rtrt-mcp binary path (optional)" style="grid-column:1 / span 2;">
    <button type="submit" style="grid-column:1 / span 2;">Render</button>
  </form>
  <pre id="setup-output" style="white-space:pre-wrap;display:none;margin-top:0.75rem;padding:0.75rem;border-radius:4px;"></pre>
</section>

<section id="stats">
  <h2>Compression</h2>
  <p style="color:#666;">Run the rule engine (level: lite/full/ultra/extreme) or the LLMLingua-style ML compressor against arbitrary text.</p>
  <form id="compress-form" style="display:grid;grid-template-columns:1fr 1fr;gap:0.5rem;max-width:720px;">
    <select id="compress-mode">
      <option value="rules">rules</option>
      <option value="ml">ml (heuristic)</option>
    </select>
    <select id="compress-level">
      <option value="lite">lite</option>
      <option value="full" selected>full</option>
      <option value="ultra">ultra</option>
      <option value="extreme">extreme</option>
    </select>
    <select id="compress-format">
      <option value="plain" selected>plain</option>
      <option value="markdown">markdown</option>
      <option value="xml">xml</option>
      <option value="json">json</option>
    </select>
    <input id="compress-ratio" type="number" step="0.05" min="0.1" max="1" value="0.5" title="ml ratio">
    <textarea id="compress-input" rows="6" placeholder="paste text to compress" required style="grid-column:1 / span 2;"></textarea>
    <button type="submit" style="grid-column:1 / span 2;">Compress</button>
  </form>
  <div id="compress-summary" style="margin-top:0.75rem;color:#666;"></div>
  <pre id="compress-output" style="white-space:pre-wrap;background:#fafafa;border:1px solid #eee;padding:0.75rem;border-radius:4px;display:none;"></pre>
</section>

<script>
function sparkline(svgId, values, color) {
  const svg = document.getElementById(svgId);
  if (!svg) return;
  if (!values.length) {
    svg.innerHTML = `<text x="50%" y="50%" text-anchor="middle" fill="var(--muted)">no data</text>`;
    return;
  }
  const w = 400, h = 120, pad = 4;
  const max = Math.max(1, ...values);
  const step = values.length > 1 ? (w - pad * 2) / (values.length - 1) : 0;
  const points = values.map((v, i) => {
    const x = pad + i * step;
    const y = h - pad - (v / max) * (h - pad * 2);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  }).join(' ');
  const fillPoints = `${pad},${h - pad} ${points} ${(pad + (values.length - 1) * step).toFixed(1)},${h - pad}`;
  svg.innerHTML =
    `<polygon points="${fillPoints}" fill="${color}" fill-opacity="0.15" stroke="none"/>` +
    `<polyline points="${points}" fill="none" stroke="${color}" stroke-width="1.5"/>` +
    `<text x="${w - pad}" y="12" text-anchor="end" fill="var(--muted)" font-size="10">max ${max}</text>`;
}
async function loadMetrics() {
  const m = await fetch('/api/metrics').then(r => r.json());
  const s = m.summary;
  document.getElementById('metrics-summary').innerHTML = `
    <span class="kpi">calls<br><b>${s.calls}</b></span>
    <span class="kpi">ok<br><b>${s.successes}</b></span>
    <span class="kpi">fail<br><b>${s.failures}</b></span>
    <span class="kpi">input tokens<br><b>${s.total_input_tokens}</b></span>
    <span class="kpi">output tokens<br><b>${s.total_output_tokens}</b></span>
    <span class="kpi">avg latency<br><b>${(s.total_latency_ms/Math.max(1,s.calls)).toFixed(0)} ms</b></span>
  `;
  const series = m.recent.slice().reverse();
  sparkline('chart-latency', series.map(r => r.latency_ms), 'var(--accent)');
  sparkline('chart-tokens', series.map(r => (r.usage.input_tokens || 0) + (r.usage.output_tokens || 0)), '#2ecc71');
  const tbody = document.querySelector('#metrics-recent tbody');
  tbody.innerHTML = m.recent.map(r => {
    const t = new Date(r.started_at * 1000).toISOString().slice(11,19);
    const status = r.ok ? 'ok' : `<span class="err">${r.error||'failed'}</span>`;
    return `<tr><td>${t}</td><td>${r.provider}</td><td><code>${r.model}</code></td><td>${r.usage.input_tokens}</td><td>${r.usage.output_tokens}</td><td>${r.latency_ms} ms</td><td>${status}</td></tr>`;
  }).join('');
}
let LOADED_TEMPLATES = [];
async function loadTemplates() {
  const tpls = await fetch('/api/templates').then(r => r.json());
  LOADED_TEMPLATES = tpls;
  document.querySelector('#tpl tbody').innerHTML = tpls.map(t =>
    `<tr><td><code>${t.name}</code></td><td>${t.source}</td><td>${t.description}</td>` +
    `<td><a data-pick="${t.name}" style="cursor:pointer;color:#2962FF;">use</a></td></tr>`
  ).join('');
  document.querySelectorAll('a[data-pick]').forEach(a => a.onclick = () => {
    document.querySelector('[data-tab="templates"]').click();
    document.getElementById('scaffold-template').value = a.dataset.pick;
    renderScaffoldVars();
  });
  const sel = document.getElementById('scaffold-template');
  sel.innerHTML = tpls.map(t => `<option value="${t.name}">${t.name}</option>`).join('');
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
      `<td><input data-var="${v.name}" placeholder="${v.description || ''}"` +
      ` value="${v.default || ''}" style="width:100%;"></td></tr>`
    ).join('') + `</tbody></table>`;
}
document.getElementById('scaffold-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const variables = {};
  document.querySelectorAll('#scaffold-vars [data-var]').forEach(inp => {
    if (inp.value) variables[inp.dataset.var] = inp.value;
  });
  const body = {
    template: document.getElementById('scaffold-template').value,
    target: document.getElementById('scaffold-target').value,
    variables,
    overwrite: document.getElementById('scaffold-overwrite').checked,
  };
  const out = document.getElementById('scaffold-result');
  out.textContent = 'scaffolding…';
  const resp = await fetch('/api/templates/scaffold', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    out.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`;
    return;
  }
  const d = await resp.json();
  out.innerHTML = `wrote ${d.files_written} files into <code>${d.root}</code>` +
    (d.post_hooks.length ? `<br>post-hooks: ${d.post_hooks.map(h => `<code>${h}</code>`).join(', ')}` : '');
};
document.getElementById('blocks-list-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const project = document.getElementById('blocks-project').value;
  const tbody = document.querySelector('#blocks-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="2" style="color:#666;">loading…</td></tr>`;
  const resp = await fetch(`/api/memory/blocks?project=${encodeURIComponent(project)}`);
  if (!resp.ok) {
    tbody.innerHTML = `<tr><td colspan="2" class="err">${resp.status}: ${await resp.text()}</td></tr>`;
    return;
  }
  const d = await resp.json();
  if (!d.blocks.length) {
    tbody.innerHTML = `<tr><td colspan="2" style="color:#666;">no blocks</td></tr>`;
    return;
  }
  tbody.innerHTML = d.blocks.map(b => {
    const name = b.kind.replace(/^block:/, '');
    return `<tr><td><code>${name}</code></td><td>${b.body.replace(/</g, '&lt;')}</td></tr>`;
  }).join('');
};
document.getElementById('blocks-set-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    project: document.getElementById('block-set-project').value,
    name: document.getElementById('block-set-name').value,
    body: document.getElementById('block-set-body').value,
  };
  const out = document.getElementById('block-set-result');
  const resp = await fetch('/api/memory/blocks', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    out.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`;
    return;
  }
  const d = await resp.json();
  out.textContent = `set id=${d.id}`;
};

document.getElementById('save-form').onsubmit = async (ev) => {
  ev.preventDefault();
  let metadata = {};
  const rawMeta = document.getElementById('save-metadata').value.trim();
  if (rawMeta) {
    try { metadata = JSON.parse(rawMeta); }
    catch (e) {
      document.getElementById('save-result').innerHTML = `<span class="err">metadata JSON: ${e}</span>`;
      return;
    }
  }
  const body = {
    project: document.getElementById('save-project').value,
    kind: document.getElementById('save-kind').value || 'note',
    body: document.getElementById('save-body').value,
    metadata,
  };
  const out = document.getElementById('save-result');
  const resp = await fetch('/api/memory/save', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    out.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`;
    return;
  }
  const data = await resp.json();
  out.textContent = `saved id=${data.id}`;
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
  sum.textContent = 'filtering…';
  pre.style.display = 'none';
  const resp = await fetch('/api/proxy', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!resp.ok) { sum.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`; return; }
  const d = await resp.json();
  const ratio = d.original_len ? ((d.filtered_len / d.original_len) * 100).toFixed(1) : '0';
  sum.textContent = `mode=${d.mode} · ${d.original_len} → ${d.filtered_len} chars (${ratio}%) · saved ${d.saved_chars}`;
  pre.style.display = 'block';
  pre.textContent = d.filtered;
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
  meta.textContent = 'diagnosing…';
  pre.style.display = 'none';
  const resp = await fetch('/api/diagnose', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!resp.ok) { meta.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`; return; }
  const d = await resp.json();
  meta.textContent = `${d.provider}/${d.model} · in ${d.input_tokens} · out ${d.output_tokens}`;
  pre.style.display = 'block';
  pre.textContent = d.diagnosis;
};
document.getElementById('repomap-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    root: document.getElementById('repomap-root').value,
    ext: document.getElementById('repomap-ext').value || '.rs',
    max_bytes: Number(document.getElementById('repomap-max').value) || 524288,
  };
  const sum = document.getElementById('repomap-summary');
  const pre = document.getElementById('repomap-output');
  sum.textContent = 'walking…';
  pre.style.display = 'none';
  const resp = await fetch('/api/repo-map', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!resp.ok) { sum.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`; return; }
  const d = await resp.json();
  sum.textContent = `${d.files} files · ${d.total_bytes} bytes scanned · ${d.signature_chars} chars of signatures`;
  pre.style.display = 'block';
  pre.textContent = d.entries.map(e => `// ${e.path}\n${e.signatures}\n`).join('\n');
};
document.getElementById('setup-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    agent: document.getElementById('setup-agent').value,
    memory: document.getElementById('setup-memory').value || null,
    binary: document.getElementById('setup-binary').value || null,
  };
  const pre = document.getElementById('setup-output');
  pre.style.display = 'none';
  const resp = await fetch('/api/setup', {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(body)});
  if (!resp.ok) { pre.style.display = 'block'; pre.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`; return; }
  const d = await resp.json();
  pre.style.display = 'block';
  pre.textContent = `# ${d.agent} → ${d.target_path}\n\n${d.snippet}`;
};
document.getElementById('compress-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const mode = document.getElementById('compress-mode').value;
  const body = {
    text: document.getElementById('compress-input').value,
    ml: mode === 'ml',
    level: document.getElementById('compress-level').value,
    format: document.getElementById('compress-format').value,
    ratio: Number(document.getElementById('compress-ratio').value),
  };
  const summary = document.getElementById('compress-summary');
  const pre = document.getElementById('compress-output');
  summary.textContent = 'compressing…';
  pre.style.display = 'none';
  const resp = await fetch('/api/compress', {
    method: 'POST',
    headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(body),
  });
  if (!resp.ok) {
    summary.innerHTML = `<span class="err">${resp.status}: ${await resp.text()}</span>`;
    return;
  }
  const d = await resp.json();
  const ratio = d.original_len ? ((d.compressed_len / d.original_len) * 100).toFixed(1) : '0';
  summary.textContent = `mode=${d.mode}${d.scorer ? ` scorer=${d.scorer}` : ''} · ${d.original_len} → ${d.compressed_len} chars (${ratio}%) · saved ${d.saved_chars}`;
  pre.style.display = 'block';
  pre.textContent = d.compressed;
};
async function loadStats() {
  // legacy stats endpoint kept for backwards compat; compression UI handles
  // the real interaction now.
  return;
}
function fmtUsd(v) { return v === null || v === undefined ? '—' : `$${Number(v).toFixed(4)}`; }
async function loadBudget() {
  const b = await fetch('/api/budget').then(r => r.json());
  const cache = b.cache_len === null || b.cache_len === undefined ? 'off' : b.cache_len;
  document.getElementById('budget-summary').innerHTML = `
    <span class="kpi">cap<br><b>${fmtUsd(b.cap_usd)}</b></span>
    <span class="kpi">spent<br><b>${fmtUsd(b.spent_usd)}</b></span>
    <span class="kpi">remaining<br><b>${fmtUsd(b.remaining_usd)}</b></span>
    <span class="kpi">cache<br><b>${cache}</b></span>
  `;
}
async function loadPrompts() {
  let prompts = [];
  try { prompts = await fetch('/api/prompts').then(r => r.ok ? r.json() : []); } catch (_) {}
  const tbody = document.querySelector('#prompts-tbl tbody');
  if (!prompts.length) {
    tbody.innerHTML = `<tr><td colspan="3" style="color:#666;">no prompts registered yet</td></tr>`;
    return;
  }
  tbody.innerHTML = prompts.map(p => {
    const versions = p.versions.map(v =>
      `<a data-name="${p.name}" data-version="${v}" style="margin-right:0.5rem;cursor:pointer;color:#2962FF;">v${v}</a>`
    ).join('');
    return `<tr><td><code>${p.name}</code></td><td>v${p.latest}</td><td>${versions}</td></tr>`;
  }).join('');
  tbody.querySelectorAll('a[data-name]').forEach(a => a.onclick = async () => {
    const body = await fetch(`/api/prompts/${a.dataset.name}/${a.dataset.version}`).then(r => r.json());
    const pre = document.getElementById('prompt-body');
    pre.style.display = 'block';
    pre.textContent = `# ${body.name} v${body.version}\n\n${body.body}`;
  });
}
(function initTheme() {
  const saved = localStorage.getItem('rtrt-theme');
  const prefersDark = window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches;
  const theme = saved || (prefersDark ? 'dark' : 'light');
  document.documentElement.setAttribute('data-theme', theme);
})();
document.getElementById('theme-toggle').onclick = () => {
  const cur = document.documentElement.getAttribute('data-theme') || 'light';
  const next = cur === 'dark' ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  localStorage.setItem('rtrt-theme', next);
};
document.querySelectorAll('nav a').forEach(a => a.onclick = () => {
  document.querySelectorAll('nav a').forEach(x => x.classList.remove('active'));
  document.querySelectorAll('section').forEach(x => x.classList.remove('active'));
  a.classList.add('active');
  const target = a.dataset.tab;
  document.getElementById(target).classList.add('active');
});
document.getElementById('recall-form').onsubmit = async (ev) => {
  ev.preventDefault();
  const body = {
    project: document.getElementById('recall-project').value,
    query: document.getElementById('recall-query').value,
    limit: Number(document.getElementById('recall-limit').value) || 10,
    filter: document.getElementById('recall-filter').value || null,
  };
  const tbody = document.querySelector('#recall-tbl tbody');
  tbody.innerHTML = `<tr><td colspan="4" style="color:#666;">searching…</td></tr>`;
  try {
    const resp = await fetch('/api/memory/recall', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(body),
    });
    if (!resp.ok) {
      tbody.innerHTML = `<tr><td colspan="4" class="err">${resp.status}: ${await resp.text()}</td></tr>`;
      return;
    }
    const data = await resp.json();
    if (!data.hits.length) {
      tbody.innerHTML = `<tr><td colspan="4" style="color:#666;">no hits</td></tr>`;
      return;
    }
    tbody.innerHTML = data.hits.map(h =>
      `<tr><td>${h.id}</td><td><code>${h.kind}</code></td><td>${h.scope}</td><td>${h.body.replace(/</g, '&lt;')}</td></tr>`
    ).join('');
  } catch (e) {
    tbody.innerHTML = `<tr><td colspan="4" class="err">${e}</td></tr>`;
  }
};
loadMetrics();
loadTemplates();
loadStats();
loadBudget();
loadPrompts();
setInterval(loadMetrics, 5000);
setInterval(loadBudget, 5000);
</script>
</body>
</html>
"#;
