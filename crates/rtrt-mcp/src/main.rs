//! rtrt-mcp — MCP server exposing the RTRT toolkit's surfaces as tools.
//!
//! Two transports:
//! - **stdio** (default) — standard MCP framing for local agent integrations.
//! - **http** — Streamable HTTP (MCP 2025-06-18) served by `rmcp`'s
//!   `StreamableHttpService` behind an axum router. Defaults to loopback for
//!   DNS-rebinding safety; the bind address is configurable.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;
use rtrt_memory::MemoryStore;
use rtrt_providers::{ChatMessage, ChatRequest, Gateway, Role};
use serde::Deserialize;
use tokio::sync::Mutex;

#[derive(Debug, Parser)]
#[command(name = "rtrt-mcp", version, about = "RTRT MCP server (stdio + http)", long_about = None)]
struct Cli {
    /// Path to the SQLite memory store. Created if missing.
    #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
    memory: PathBuf,
    /// Transport. `stdio` is the default for local agent integrations;
    /// `http` exposes the Streamable HTTP transport over an axum router.
    #[arg(long, value_enum, default_value = "stdio")]
    transport: Transport,
    /// Bind address for `--transport http`.
    #[arg(long, default_value = "127.0.0.1:7312")]
    bind: String,
    /// HTTP mount path for the MCP endpoint.
    #[arg(long, default_value = "/mcp")]
    path: String,
    /// Required bearer token for `--transport http`. Without it the server
    /// rejects every request with 401. Reading from the environment keeps
    /// the secret out of the process listing.
    #[arg(long, env = "RTRT_MCP_HTTP_TOKEN")]
    http_token: Option<String>,
    /// Allowed browser Origins (comma-separated) for `--transport http`.
    /// Empty disables Origin validation; non-empty enables it per RFC 6454.
    #[arg(long, env = "RTRT_MCP_ALLOWED_ORIGINS", value_delimiter = ',')]
    allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[derive(Clone)]
struct RtrtMcp {
    // Populated by rmcp's #[tool_router] macro; read via Self::tool_router() so
    // the field looks unused to dead-code analysis.
    #[allow(dead_code)]
    tool_router: ToolRouter<RtrtMcp>,
    state: Arc<RtrtState>,
}

struct RtrtState {
    memory: Mutex<MemoryStore>,
    gateway: Arc<Gateway>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompressArgs {
    /// Text to compress.
    text: String,
    /// One of `lite`, `full`, `ultra`. Defaults to `full`.
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySaveArgs {
    project: String,
    #[serde(default = "default_kind")]
    kind: String,
    body: String,
}

fn default_kind() -> String {
    "note".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RepoMapArgs {
    /// Root directory to walk.
    root: PathBuf,
    /// Skip files larger than this many bytes. Defaults to 524288.
    #[serde(default)]
    max_bytes: Option<u64>,
    /// Restrict to files ending with this suffix (e.g. `.rs`). Empty =
    /// auto-detect every supported language.
    #[serde(default)]
    ext: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompressMlArgs {
    /// Text to compress.
    text: String,
    /// Target ratio (kept-token fraction) in (0.05, 1.0]. Defaults to 0.5.
    #[serde(default)]
    ratio: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProxyArgs {
    /// Raw output to filter.
    raw: String,
    /// Optional command (e.g. `git status`, `cargo build`) — picks a
    /// command-specific filter from `rtrt-proxy::FILTERS`.
    #[serde(default)]
    command: Option<String>,
    /// Mode override: `command` (default), `errors_only`, `ultra_compact`.
    #[serde(default)]
    mode: Option<String>,
    /// Context-line count for `errors_only` (default 3).
    #[serde(default)]
    context: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryRecallArgs {
    project: String,
    query: String,
    #[serde(default = "default_limit")]
    limit: u32,
    /// Optional qdrant-style payload filter — e.g. `source=claude,topic~^auth`.
    #[serde(default)]
    filter: Option<String>,
}

fn default_limit() -> u32 {
    5
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySetBlockArgs {
    project: String,
    /// Block name. Typical: `persona`, `human`, `context`. Free-form slug.
    name: String,
    body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryGetBlockArgs {
    project: String,
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryListBlocksArgs {
    project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProviderChatArgs {
    /// Model id (e.g. `claude-haiku-4-5`, `gpt-5.4-mini`, `llama3.2`).
    model: String,
    /// Messages in order; roles are `system` / `user` / `assistant`.
    messages: Vec<ProviderChatMessage>,
    /// Optional max tokens (defaults to 1024 in the Anthropic adapter).
    #[serde(default)]
    max_tokens: Option<u32>,
    /// Optional sampling temperature.
    #[serde(default)]
    temperature: Option<f32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProviderChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TemplatesScaffoldArgs {
    template: String,
    target: PathBuf,
    #[serde(default)]
    variables: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    overwrite: bool,
}

#[tool_router]
impl RtrtMcp {
    /// Build from an already-shared state — same state is reused across stdio
    /// and HTTP transports so every session shares one SQLite handle + one
    /// gateway.
    pub fn with_state(state: Arc<RtrtState>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            state,
        }
    }

    #[tool(
        description = "Compress text via the RTRT caveman-style rewriter. Levels: lite, full, ultra."
    )]
    fn compress(
        &self,
        Parameters(args): Parameters<CompressArgs>,
    ) -> Result<CallToolResult, McpError> {
        let level = match args.level.as_deref().unwrap_or("full") {
            "lite" => CompressionLevel::Lite,
            "full" => CompressionLevel::Full,
            "ultra" => CompressionLevel::Ultra,
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown level: {other}"),
                    None,
                ));
            }
        };
        let out = Compressor::new(level).compress(&args.text);
        let body = serde_json::json!({
            "compressed": out,
            "saved_chars": args.text.chars().count().saturating_sub(out.chars().count()),
            "original_len": args.text.chars().count(),
            "compressed_len": out.chars().count(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "LLMLingua-style ML compression. Keeps roughly `ratio` of the input tokens by token-importance scoring (heuristic backend until real ONNX scorer lands)."
    )]
    fn compress_ml(
        &self,
        Parameters(args): Parameters<CompressMlArgs>,
    ) -> Result<CallToolResult, McpError> {
        let target = rtrt_compress::CompressionTarget::new(args.ratio.unwrap_or(0.5))
            .map_err(|e| McpError::invalid_params(format!("compress_ml: {e}"), None))?;
        let compressor = rtrt_compress::MlCompressor::heuristic();
        let out = compressor.compress(&args.text, target);
        let body = serde_json::json!({
            "compressed": out,
            "scorer": compressor.scorer_name(),
            "original_len": args.text.chars().count(),
            "compressed_len": out.chars().count(),
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Filter command output via rtrt-proxy. Modes: `command` (matches the command label), `errors_only` (keeps error/warning lines + context), `ultra_compact` (collapses repeated lines)."
    )]
    fn proxy(&self, Parameters(args): Parameters<ProxyArgs>) -> Result<CallToolResult, McpError> {
        let mode = args.mode.as_deref().unwrap_or("command");
        let context = args.context.unwrap_or(3) as usize;
        let original = args.raw.chars().count();
        let out = match mode {
            "command" => {
                let cmd = args.command.as_deref().ok_or_else(|| {
                    McpError::invalid_params("command required for command-mode", None)
                })?;
                match rtrt_proxy::filter_for(cmd) {
                    Some(f) => f.apply(&args.raw),
                    None => args.raw.clone(),
                }
            }
            "errors_only" => rtrt_proxy::errors_only(&args.raw, context),
            "ultra_compact" => rtrt_proxy::ultra_compact(&args.raw),
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown proxy mode: {other}"),
                    None,
                ));
            }
        };
        let filtered = out.chars().count();
        let body = serde_json::json!({
            "filtered": out,
            "mode": mode,
            "original_len": original,
            "filtered_len": filtered,
            "saved_chars": original.saturating_sub(filtered),
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(description = "Save a memory record to the SQLite store. Returns the new id.")]
    async fn memory_save(
        &self,
        Parameters(args): Parameters<MemorySaveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let id = store
            .save(&args.project, &args.kind, &args.body)
            .map_err(|e| McpError::internal_error(format!("memory.save: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::json!({ "id": id }).to_string(),
        )]))
    }

    #[tool(description = "Recall memories by BM25 (FTS5) for a project.")]
    async fn memory_recall(
        &self,
        Parameters(args): Parameters<MemoryRecallArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let hits = match args.filter.as_deref() {
            Some(spec) if !spec.is_empty() => {
                let filter = rtrt_memory::PayloadFilter::parse(spec).map_err(|e| {
                    McpError::invalid_params(format!("memory.recall filter: {e}"), None)
                })?;
                store
                    .recall_bm25_with_filter(
                        &args.project,
                        &args.query,
                        args.limit as usize,
                        &filter,
                    )
                    .map_err(|e| McpError::internal_error(format!("memory.recall: {e}"), None))?
            }
            _ => store
                .recall_bm25(&args.project, &args.query, args.limit as usize)
                .map_err(|e| McpError::internal_error(format!("memory.recall: {e}"), None))?,
        };
        let body = serde_json::to_value(&hits)
            .map_err(|e| McpError::internal_error(format!("memory.recall serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Walk a directory and emit a tree-sitter signature map of every supported source file (.rs / .py / .ts / .tsx). Bodies are stripped; the result is the API surface."
    )]
    fn repo_map(
        &self,
        Parameters(args): Parameters<RepoMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        let max_bytes = args.max_bytes.unwrap_or(524_288);
        let restrict_ext = args.ext.unwrap_or_default();
        if !args.root.exists() {
            return Err(McpError::invalid_params(
                format!("root not found: {}", args.root.display()),
                None,
            ));
        }
        let mut files = 0usize;
        let mut total_bytes: u64 = 0;
        let mut signature_chars: usize = 0;
        let mut entries: Vec<serde_json::Value> = Vec::new();
        for entry in walk_files(&args.root) {
            let name = entry.to_string_lossy();
            if !restrict_ext.is_empty() && !name.ends_with(&restrict_ext) {
                continue;
            }
            let Some(lang) = rtrt_compress::Language::from_filename(&name) else {
                continue;
            };
            let size = std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
            if size > max_bytes {
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
            files += 1;
            let rel = entry
                .strip_prefix(&args.root)
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
        let body = serde_json::json!({
            "files": files,
            "total_bytes": total_bytes,
            "signature_chars": signature_chars,
            "entries": entries,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(description = "List built-in and custom project templates.")]
    fn templates_list(&self) -> Result<CallToolResult, McpError> {
        let templates = rtrt_templates::list_all();
        let body = serde_json::to_value(
            templates
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "source": format!("{:?}", t.source),
                        "variables": t.variables,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .map_err(|e| McpError::internal_error(format!("templates.list serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Scaffold a project from a template. Variables substitute `{{key}}` placeholders."
    )]
    fn templates_scaffold(
        &self,
        Parameters(args): Parameters<TemplatesScaffoldArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tmpl = rtrt_templates::find(&args.template).ok_or_else(|| {
            McpError::invalid_params(format!("unknown template: {}", args.template), None)
        })?;
        let plan = rtrt_templates::render::plan(&tmpl, &args.target, args.variables)
            .map_err(|e| McpError::internal_error(format!("templates.scaffold plan: {e}"), None))?;
        rtrt_templates::render::write(&plan, args.overwrite).map_err(|e| {
            McpError::internal_error(format!("templates.scaffold write: {e}"), None)
        })?;
        let body = serde_json::json!({
            "files_written": plan.files.len(),
            "root": plan.root,
            "post_hooks": plan.post_hooks,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Set a Letta-style memory block. Overwrites any existing block with the same name."
    )]
    async fn memory_set_block(
        &self,
        Parameters(args): Parameters<MemorySetBlockArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let id = store
            .set_block(&args.project, &args.name, &args.body)
            .map_err(|e| McpError::internal_error(format!("memory.set_block: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::json!({ "id": id }).to_string(),
        )]))
    }

    #[tool(description = "Get a Letta-style memory block by name. Returns null when missing.")]
    async fn memory_get_block(
        &self,
        Parameters(args): Parameters<MemoryGetBlockArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let block = store
            .get_block(&args.project, &args.name)
            .map_err(|e| McpError::internal_error(format!("memory.get_block: {e}"), None))?;
        let body = serde_json::to_value(&block)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(description = "List every Letta-style memory block in the project.")]
    async fn memory_list_blocks(
        &self,
        Parameters(args): Parameters<MemoryListBlocksArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let blocks = store
            .list_blocks(&args.project)
            .map_err(|e| McpError::internal_error(format!("memory.list_blocks: {e}"), None))?;
        let body = serde_json::to_value(&blocks)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Chat with a registered provider via the gateway. Routes by model id (claude-* → anthropic, gpt-*/o* → openai, otherwise the openai-compat fallback)."
    )]
    async fn provider_chat(
        &self,
        Parameters(args): Parameters<ProviderChatArgs>,
    ) -> Result<CallToolResult, McpError> {
        let messages = args
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
        let req = ChatRequest {
            model: args.model,
            messages,
            max_tokens: args.max_tokens,
            temperature: args.temperature,
        };
        let resp = self
            .state
            .gateway
            .chat(req)
            .await
            .map_err(|e| McpError::internal_error(format!("provider.chat: {e}"), None))?;
        let body = serde_json::json!({
            "provider": resp.provider,
            "model": resp.model,
            "content": resp.content,
            "input_tokens": resp.usage.input_tokens,
            "output_tokens": resp.usage.output_tokens,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }
}

/// Walk a directory tree, skipping `target/` and dot-prefixed entries.
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

/// Bearer-token guard for the HTTP transport. When `expected` is `None` every
/// request is admitted (the operator opted out by omitting `--http-token`).
async fn bearer_guard(
    expected: Option<Arc<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{HeaderValue, StatusCode, header::AUTHORIZATION};
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
        HeaderValue::from_static("Bearer realm=\"rtrt-mcp\""),
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

#[tool_handler]
impl ServerHandler for RtrtMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "RTRT MCP server. Tools: compress (caveman-style rewriter), \
                 compress_ml (LLMLingua-style token-importance compression), \
                 proxy (rtrt-proxy command/errors_only/ultra_compact filters), \
                 repo_map (tree-sitter signature index for .rs / .py / .ts / .tsx), \
                 memory_save / memory_recall (SQLite + FTS5 BM25; recall accepts a qdrant-style payload filter), \
                 memory_set_block / memory_get_block / memory_list_blocks (Letta-style blocks), \
                 templates_list / templates_scaffold (built-in project scaffolds), \
                 provider_chat (multi-provider gateway dispatch).",
            )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("rtrt=info,rmcp=info")
        .with_ansi(false)
        .init();
    let cli = Cli::parse();
    let memory = MemoryStore::open(&cli.memory)?;
    let gateway = Arc::new(Gateway::from_env());
    let shared_state = Arc::new(RtrtState {
        memory: Mutex::new(memory),
        gateway,
    });
    match cli.transport {
        Transport::Stdio => {
            tracing::info!(
                "rtrt-mcp starting on stdio; memory={}",
                cli.memory.display()
            );
            let service = RtrtMcp::with_state(shared_state).serve(stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http => {
            tracing::info!(
                "rtrt-mcp starting on http://{}{}; memory={}; auth={}; origins={}",
                cli.bind,
                cli.path,
                cli.memory.display(),
                if cli.http_token.is_some() {
                    "bearer"
                } else {
                    "open"
                },
                if cli.allowed_origins.is_empty() {
                    "*".into()
                } else {
                    cli.allowed_origins.join(",")
                },
            );
            if cli.http_token.is_none()
                && !cli.bind.starts_with("127.")
                && !cli.bind.starts_with("[::1]")
                && !cli.bind.starts_with("localhost")
            {
                tracing::warn!(
                    "binding {} without --http-token is risky; non-loopback callers can hit the MCP endpoint without authentication.",
                    cli.bind
                );
            }
            let factory_state = shared_state.clone();
            let mut config = StreamableHttpServerConfig::default();
            config.allowed_origins = cli.allowed_origins.clone();
            let mcp_service = StreamableHttpService::new(
                move || Ok(RtrtMcp::with_state(factory_state.clone())),
                Arc::new(LocalSessionManager::default()),
                config,
            );
            let token = cli.http_token.clone().map(Arc::new);
            let app = axum::Router::new()
                .route_service(&cli.path, mcp_service)
                .layer(axum::middleware::from_fn(move |req, next| {
                    let token = token.clone();
                    async move { bearer_guard(token, req, next).await }
                }));
            let listener = match tokio::net::TcpListener::bind(&cli.bind).await {
                Ok(l) => l,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                    let port = cli.bind.rsplit(':').next().unwrap_or("7312").to_string();
                    anyhow::bail!(
                        "address {bind} is already in use. Free the port (lsof -i :{port}) or pass --bind 127.0.0.1:<other> (or set --bind via env).",
                        bind = cli.bind,
                    );
                }
                Err(e) => return Err(e.into()),
            };
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_basic_cases() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
