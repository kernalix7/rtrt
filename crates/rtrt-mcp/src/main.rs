//! rtrt-mcp — MCP server exposing the RTRT toolkit's surfaces as tools.
//!
//! v0.2 ships the stdio transport via the official Rust MCP SDK (`rmcp`). The
//! tools wrap the public APIs of `rtrt-compress`, `rtrt-memory`, and
//! `rtrt-templates` — no behaviour is reimplemented inside this crate. HTTP/SSE
//! transport is on the v0.3 roadmap.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;
use rtrt_memory::MemoryStore;
use rtrt_providers::{ChatMessage, ChatRequest, Gateway, Role};
use serde::Deserialize;
use tokio::sync::Mutex;

#[derive(Debug, Parser)]
#[command(name = "rtrt-mcp", version, about = "RTRT MCP server (stdio)", long_about = None)]
struct Cli {
    /// Path to the SQLite memory store. Created if missing.
    #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
    memory: PathBuf,
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
}

fn default_limit() -> u32 {
    5
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
    pub fn new(memory: MemoryStore, gateway: Arc<Gateway>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            state: Arc::new(RtrtState {
                memory: Mutex::new(memory),
                gateway,
            }),
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
        let hits = store
            .recall_bm25(&args.project, &args.query, args.limit as usize)
            .map_err(|e| McpError::internal_error(format!("memory.recall: {e}"), None))?;
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
    tracing::info!(
        "rtrt-mcp starting on stdio; memory={}",
        cli.memory.display()
    );
    let service = RtrtMcp::new(memory, gateway).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
