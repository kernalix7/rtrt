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
    #[arg(long, default_value = "127.0.0.1:3112")]
    bind: String,
    /// HTTP mount path for the MCP endpoint.
    #[arg(long, default_value = "/mcp")]
    path: String,
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

#[tool_handler]
impl ServerHandler for RtrtMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "RTRT MCP server. Tools: compress (caveman-style rewriter), \
                 memory_save / memory_recall (SQLite + FTS5 BM25), \
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
                "rtrt-mcp starting on http://{}{}; memory={}",
                cli.bind,
                cli.path,
                cli.memory.display()
            );
            let factory_state = shared_state.clone();
            let mcp_service = StreamableHttpService::new(
                move || Ok(RtrtMcp::with_state(factory_state.clone())),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default(),
            );
            let app = axum::Router::new().route_service(&cli.path, mcp_service);
            let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}
