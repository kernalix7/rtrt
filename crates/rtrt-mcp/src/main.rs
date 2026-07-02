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
use rtrt_core::{Capability, CompressionLevel};
use rtrt_memory::{Embedder, MemoryStore};
use rtrt_providers::{
    ChatMessage, ChatRequest, DEFAULT_TIMEOUT_SECS, Gateway, InvokeOptions, Mode as InvokeMode,
    Prefer, Role, RouteRequest, UsageSnapshot, invoke_agent, select_route,
};
use rtrt_templates::PromptRegistry;
use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "rtrt-mcp", version, about = "RTRT MCP server (stdio + http)", long_about = None)]
struct Cli {
    /// Path to the SQLite memory store. Created if missing. Defaults to
    /// `~/.rtrt/memory.sqlite` — the same store as the CLI, hooks, and
    /// dashboard.
    #[arg(long, env = "RTRT_MEMORY_PATH", default_value_os_t = rtrt_core::default_memory_store_path())]
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

/// Bound on how long `memory_recall` / `memory_smart_search` will wait for a
/// hybrid (BM25 + vector RRF) attempt before falling back to plain BM25 —
/// same budget the CLI's `UserPromptSubmit` hook uses, so a slow/unreachable
/// Ollama can't stall a tool call.
const HYBRID_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Bound on the one-time startup probe that decides whether an embedder gets
/// attached at all. Cheap TCP connect only — no HTTP round-trip, no embed
/// call — so it doesn't meaningfully delay server startup either way.
const EMBED_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(700);

struct RtrtState {
    memory: Mutex<MemoryStore>,
    /// Path the primary `memory` store was opened from. Kept so background
    /// work (hybrid recall's bounded worker, the opportunistic embed sweep)
    /// can open its OWN connection instead of contending with `memory`'s lock
    /// for the duration of a possibly-slow Ollama call.
    memory_path: PathBuf,
    /// Config snapshot taken at startup (mirrors the dashboard's auto-embed
    /// daemon, which also reads config once rather than per-cycle). A config
    /// edit while the server is running takes effect on the next restart.
    cfg: rtrt_core::Config,
    /// Attached iff `[embeddings] enabled` (config/env) AND the startup probe
    /// found Ollama reachable. `None` keeps every memory tool pure BM25 with
    /// zero Ollama traffic.
    embedder: Option<Arc<dyn Embedder>>,
    gateway: Arc<Gateway>,
    prompts: Option<Arc<PromptRegistry>>,
    auto_capture: bool,
    auto_redact: bool,
    default_project: String,
    session_id: String,
    dedup_window_sec: i64,
}

impl RtrtState {
    /// Best-effort hybrid recall for `memory_recall` / `memory_smart_search`.
    /// Mirrors the CLI hook's gate (`rtrt_memory::hybrid_recall_ready`:
    /// embeddings enabled + meaningful project coverage) and timeout
    /// discipline (bounded worker, `None` on any gate miss / error /
    /// timeout — the caller's signal to fall back to `recall_bm25`).
    /// Returns `None` immediately, with zero Ollama traffic, when no embedder
    /// was attached at startup.
    async fn try_hybrid_recall(
        &self,
        project: &str,
        query: &str,
        limit: usize,
    ) -> Option<Vec<rtrt_memory::MemoryRecord>> {
        let embedder = self.embedder.clone()?;
        let cfg = self.cfg.clone();
        let memory_path = self.memory_path.clone();
        let project = project.to_string();
        let query = query.to_string();
        let task =
            tokio::task::spawn_blocking(move || -> Option<Vec<rtrt_memory::MemoryRecord>> {
                let store = MemoryStore::open(&memory_path).ok()?;
                if !rtrt_memory::hybrid_recall_ready(&store, &project, &cfg) {
                    return None;
                }
                let scored = store
                    .recall_hybrid(&project, &query, limit, embedder.as_ref())
                    .ok()?;
                Some(scored.into_iter().map(|s| s.record).collect())
            });
        match tokio::time::timeout(HYBRID_TIMEOUT, task).await {
            Ok(Ok(hits)) => hits,
            _ => None,
        }
    }

    /// Fire-and-forget incremental embed sweep after a save, so a project's
    /// embedding coverage climbs even when the dashboard's periodic
    /// auto-embed daemon isn't running (bare MCP usage). No-op when no
    /// embedder is attached. Unlike the CLI's bounded best-effort sweep (its
    /// process exits right after the command), the MCP server keeps running,
    /// so this can simply finish on its own time on a blocking task detached
    /// from the tool call — never adds latency to `memory_save`'s response.
    fn spawn_opportunistic_embed_sweep(&self) {
        let Some(embedder) = self.embedder.clone() else {
            return;
        };
        let memory_path = self.memory_path.clone();
        tokio::task::spawn_blocking(move || match MemoryStore::open(&memory_path) {
            Ok(store) => {
                let embedded = store.opportunistic_embed_sweep(embedder.as_ref());
                if embedded > 0 {
                    tracing::info!(embedded, "mcp opportunistic embed sweep");
                }
            }
            Err(e) => tracing::warn!("mcp opportunistic embed sweep: open store: {e}"),
        });
    }

    /// Best-effort capture mirroring the dashboard pipeline:
    /// `redact_secrets` → SHA-256 dedup → save → tag(session, sha).
    /// Skipped when `auto_capture` is off. Errors are swallowed so a memory
    /// hiccup never breaks the tool call that triggered it.
    async fn auto_capture(&self, kind: &str, project_override: Option<&str>, body: &str) {
        if !self.auto_capture {
            return;
        }
        let project = project_override.unwrap_or(self.default_project.as_str());
        let filtered = if self.auto_redact {
            rtrt_compress::redact_secrets(body)
        } else {
            body.to_string()
        };
        let sha = MemoryStore::body_sha(&filtered);
        let store = self.memory.lock().await;
        if self.dedup_window_sec > 0
            && let Ok(Some(seen_at)) = store.body_seen_at(project, &sha)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if now.saturating_sub(seen_at) < self.dedup_window_sec {
                return;
            }
        }
        let Ok(id) = store.save(project, kind, &filtered) else {
            return;
        };
        let _ = store.tag_row(id, Some(&self.session_id), Some(&sha));
    }
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
struct MemoryTimelineArgs {
    project: String,
    #[serde(default = "default_timeline_limit")]
    limit: u32,
    #[serde(default)]
    offset: u32,
}

fn default_timeline_limit() -> u32 {
    50
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryProjectArgs {
    project: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SecurityScanArgs {
    /// Security profile name (e.g. `ai-default`, `owasp-top-10`). See the
    /// builtin set with the `security profile list` CLI command.
    profile: String,
    /// Directory to scan. Defaults to the current working directory.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryRelationsArgs {
    project: String,
    seed_ids: Vec<i64>,
    #[serde(default = "default_depth")]
    depth: u32,
}

fn default_depth() -> u32 {
    2
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySmartSearchArgs {
    project: String,
    query: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryConsolidateArgs {
    project: String,
    #[serde(default = "default_keep")]
    keep: u32,
}

fn default_keep() -> u32 {
    20
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySessionsArgs {
    project: String,
    /// Optional `session_id`. When set, returns the rows in that session
    /// instead of the per-session summary list.
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default = "default_timeline_limit")]
    limit: u32,
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
struct AgentCallArgs {
    target: String,
    prompt: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentRouteArgs {
    prompt: String,
    #[serde(default)]
    prefer: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    model: Option<String>,
}

fn parse_agent_route_prefer(value: Option<&str>) -> Result<Prefer, McpError> {
    let Some(value) = value else {
        return Ok(Prefer::Cheapest);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "cheapest" => Ok(Prefer::Cheapest),
        "local" => Ok(Prefer::Local),
        "quality" => Ok(Prefer::Quality),
        other => Err(McpError::invalid_params(
            format!("agent_route prefer: unknown prefer '{other}'"),
            None,
        )),
    }
}

fn parse_agent_route_capability(value: Option<&str>) -> Result<Option<Capability>, McpError> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "code" => Ok(Some(Capability::Code)),
        "reasoning" => Ok(Some(Capability::Reasoning)),
        "vision" => Ok(Some(Capability::Vision)),
        "embed" => Ok(Some(Capability::Embed)),
        "agentic" => Ok(Some(Capability::Agentic)),
        "cheap" => Ok(Some(Capability::CheapBulk)),
        other => Err(McpError::invalid_params(
            format!(
                "agent_route capability: unknown capability '{other}' \
                 (expected code, reasoning, vision, embed, agentic, or cheap)"
            ),
            None,
        )),
    }
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
        description = "Compress text via the RTRT Output Optimizer rule-based rewriter. Levels: lite, full, ultra."
    )]
    async fn compress(
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
        self.state.auto_capture("compress", None, &out).await;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "LLMLingua-style ML compression. Keeps roughly `ratio` of the input tokens by token-importance scoring (heuristic backend until real ONNX scorer lands)."
    )]
    async fn compress_ml(
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
        self.state.auto_capture("compress_ml", None, &out).await;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Filter command output via rtrt-proxy. Modes: `command` (matches the command label), `errors_only` (keeps error/warning lines + context), `ultra_compact` (collapses repeated lines)."
    )]
    async fn proxy(
        &self,
        Parameters(args): Parameters<ProxyArgs>,
    ) -> Result<CallToolResult, McpError> {
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
        self.state.auto_capture("proxy", None, &out).await;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(description = "Save a memory record to the SQLite store. Returns the new id.")]
    async fn memory_save(
        &self,
        Parameters(args): Parameters<MemorySaveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let id = {
            let store = self.state.memory.lock().await;
            store
                .save(&args.project, &args.kind, &args.body)
                .map_err(|e| McpError::internal_error(format!("memory.save: {e}"), None))?
        };
        // Grow embedding coverage opportunistically; no-op without an
        // embedder, and never adds latency to this response either way.
        self.state.spawn_opportunistic_embed_sweep();
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::json!({ "id": id }).to_string(),
        )]))
    }

    #[tool(
        description = "Recall memories for a project: hybrid (BM25 + vector RRF) when a local embedder is attached and project embedding coverage is meaningful, else plain BM25 (FTS5). An optional payload filter always uses BM25 (the hybrid + filter combo isn't implemented yet)."
    )]
    async fn memory_recall(
        &self,
        Parameters(args): Parameters<MemoryRecallArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(spec) = args.filter.as_deref().filter(|s| !s.is_empty()) {
            let filter = rtrt_memory::PayloadFilter::parse(spec).map_err(|e| {
                McpError::invalid_params(format!("memory.recall filter: {e}"), None)
            })?;
            let store = self.state.memory.lock().await;
            let hits = store
                .recall_bm25_with_filter(&args.project, &args.query, args.limit as usize, &filter)
                .map_err(|e| McpError::internal_error(format!("memory.recall: {e}"), None))?;
            let body = serde_json::to_value(&hits).map_err(|e| {
                McpError::internal_error(format!("memory.recall serialize: {e}"), None)
            })?;
            return Ok(CallToolResult::success(vec![Content::text(
                body.to_string(),
            )]));
        }
        let hits = match self
            .state
            .try_hybrid_recall(&args.project, &args.query, args.limit as usize)
            .await
        {
            Some(hits) => hits,
            None => {
                let store = self.state.memory.lock().await;
                store
                    .recall_bm25(&args.project, &args.query, args.limit as usize)
                    .map_err(|e| McpError::internal_error(format!("memory.recall: {e}"), None))?
            }
        };
        let body = serde_json::to_value(&hits)
            .map_err(|e| McpError::internal_error(format!("memory.recall serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Chronological feed of memories for a project, newest first. Paginate with limit + offset; returns { items, total }."
    )]
    async fn memory_timeline(
        &self,
        Parameters(args): Parameters<MemoryTimelineArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let items = store
            .recent_paged(&args.project, args.limit as usize, args.offset as usize)
            .map_err(|e| McpError::internal_error(format!("memory.timeline: {e}"), None))?;
        let total = store
            .count_by_project(&args.project)
            .map_err(|e| McpError::internal_error(format!("memory.timeline: {e}"), None))?;
        let body = serde_json::json!({
            "items": items,
            "total": total,
            "limit": args.limit,
            "offset": args.offset,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Project intelligence: every project with its memory count and most-recent save timestamp. Use as a picker before drilling into one."
    )]
    async fn memory_profile(
        &self,
        Parameters(args): Parameters<MemoryProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let projects = store
            .projects()
            .map_err(|e| McpError::internal_error(format!("memory.profile: {e}"), None))?;
        let row = projects
            .into_iter()
            .find(|(p, _, _)| p == &args.project)
            .map(|(p, count, latest)| {
                serde_json::json!({ "project": p, "count": count, "latest_ts": latest })
            })
            .unwrap_or_else(
                || serde_json::json!({ "project": args.project, "count": 0, "latest_ts": 0 }),
            );
        Ok(CallToolResult::success(vec![Content::text(
            row.to_string(),
        )]))
    }

    #[tool(
        description = "Knowledge graph traversal from one or more seed memory ids. Walks edges within `depth` hops, staying inside `project` and capped to a data-scaled visit budget. Returns every reached memory record."
    )]
    async fn memory_relations(
        &self,
        Parameters(args): Parameters<MemoryRelationsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        // Project scoping happens inside the traversal (foreign rows never
        // act as bridges) and the walk is visit-capped in the store.
        let items = store
            .recall_via_graph_scoped(&args.seed_ids, args.depth, Some(&args.project))
            .map_err(|e| McpError::internal_error(format!("memory.relations: {e}"), None))?;
        let body = serde_json::to_value(&items).map_err(|e| {
            McpError::internal_error(format!("memory.relations serialize: {e}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Hybrid (BM25 + vector RRF) search when a local embedder is attached and project embedding coverage is meaningful. Falls back to plain BM25 otherwise. Returns the top `limit` records."
    )]
    async fn memory_smart_search(
        &self,
        Parameters(args): Parameters<MemorySmartSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let hits = match self
            .state
            .try_hybrid_recall(&args.project, &args.query, args.limit as usize)
            .await
        {
            Some(hits) => hits,
            None => {
                let store = self.state.memory.lock().await;
                store
                    .recall_bm25(&args.project, &args.query, args.limit as usize)
                    .map_err(|e| {
                        McpError::internal_error(format!("memory.smart_search: {e}"), None)
                    })?
            }
        };
        let body = serde_json::to_value(&hits)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Export every memory row for `project` as JSON Lines, returned as one string in the response body."
    )]
    async fn memory_export(
        &self,
        Parameters(args): Parameters<MemoryProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let mut buf: Vec<u8> = Vec::new();
        store
            .export_jsonl(&args.project, &mut buf)
            .map_err(|e| McpError::internal_error(format!("memory.export: {e}"), None))?;
        let body = String::from_utf8_lossy(&buf).into_owned();
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "No-LLM consolidation sweep on `project`: keep the most recent `keep` memories untouched, roll every older row into one archival digest row (kind `archival`, payload `archive=true`, one preview line per archived row), then delete the originals. Returns the digest row id."
    )]
    async fn memory_consolidate(
        &self,
        Parameters(args): Parameters<MemoryConsolidateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let (removed, digest_id) = store
            .archive_overflow_no_llm(&args.project, args.keep as usize)
            .map_err(|e| McpError::internal_error(format!("memory.consolidate: {e}"), None))?;
        let after = store
            .count_by_project(&args.project)
            .map_err(|e| McpError::internal_error(format!("memory.consolidate: {e}"), None))?;
        let body = serde_json::json!({
            "project": args.project,
            "removed": removed,
            "digest_id": digest_id,
            "kept": after,
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "List sessions for `project` (one row per `session_id` with count + first/last timestamps), or — if `session_id` is supplied — return the memory rows for that session newest-first."
    )]
    async fn memory_sessions(
        &self,
        Parameters(args): Parameters<MemorySessionsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.memory.lock().await;
        let body = if let Some(sid) = args.session_id.as_deref() {
            let rows = store
                .session_records(&args.project, sid, args.limit as usize)
                .map_err(|e| McpError::internal_error(format!("memory.sessions: {e}"), None))?;
            let items: Vec<_> = rows
                .into_iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "kind": r.kind,
                        "body": r.body,
                        "created_at": r.created_at,
                    })
                })
                .collect();
            serde_json::json!({
                "project": args.project,
                "session_id": sid,
                "items": items,
            })
        } else {
            let summaries = store
                .sessions(&args.project)
                .map_err(|e| McpError::internal_error(format!("memory.sessions: {e}"), None))?;
            let items: Vec<_> = summaries
                .into_iter()
                .map(|(sid, n, first, last)| {
                    serde_json::json!({
                        "session_id": sid,
                        "count": n,
                        "first_ts": first,
                        "last_ts": last,
                    })
                })
                .collect();
            serde_json::json!({
                "project": args.project,
                "sessions": items,
            })
        };
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
        self.state
            .auto_capture("provider_chat", None, &resp.content)
            .await;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Invoke a detected local agent or provider through RTRT's cross-tool bridge. mode: cli, api, or auto."
    )]
    async fn agent_call(
        &self,
        Parameters(args): Parameters<AgentCallArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mode = match args.mode.as_deref() {
            Some(value) => Some(
                InvokeMode::parse_label(value)
                    .map_err(|e| McpError::invalid_params(format!("agent_call mode: {e}"), None))?,
            ),
            None => Some(InvokeMode::Auto),
        };
        let outcome = invoke_agent(
            &args.target,
            &args.prompt,
            InvokeOptions {
                mode,
                model: args.model,
                timeout: std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            },
        )
        .await
        .map_err(|e| McpError::internal_error(format!("agent_call: {e}"), None))?;
        let body = serde_json::to_string(&outcome)
            .map_err(|e| McpError::internal_error(format!("agent_call serialize: {e}"), None))?;
        self.state
            .auto_capture("agent_call", None, &outcome.output)
            .await;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Select the best detected agent route by preference and capability, optionally invoking the selected target."
    )]
    async fn agent_route(
        &self,
        Parameters(args): Parameters<AgentRouteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let req = RouteRequest {
            capability: parse_agent_route_capability(args.capability.as_deref())?,
            prefer: parse_agent_route_prefer(args.prefer.as_deref())?,
            target: None,
            model: args.model,
            mode: None,
        };
        // Same routing inputs as the CLI: the effective (global ⊕ project
        // `.rtrt/config.toml`) config drives the per-project enable map, and
        // the usage snapshot carries the ledger's rolling 24h window so
        // ranking is headroom-aware here too.
        let cfg = rtrt_core::Config::load_effective_for_cwd();
        let tools = rtrt_core::detect_tools_with_config(cfg);
        let usage = UsageSnapshot::load_for_routing();
        let decision = select_route(&req, &tools, &usage)
            .map_err(|e| McpError::internal_error(format!("agent_route: {e}"), None))?;
        let target = decision.target.clone();
        let mut body = serde_json::json!({
            "target": target,
            "cost_class": decision.cost_class,
            "reason": decision.reason.clone(),
            "alternatives": decision.alternatives.clone(),
        });
        if args.dry_run.unwrap_or(true) {
            return Ok(CallToolResult::success(vec![Content::text(
                body.to_string(),
            )]));
        }
        let outcome = invoke_agent(
            &decision.target,
            &args.prompt,
            InvokeOptions {
                mode: Some(decision.mode),
                model: decision.model,
                timeout: std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            },
        )
        .await
        .map_err(|e| McpError::internal_error(format!("agent_route: {e}"), None))?;
        body["output"] = serde_json::Value::String(rtrt_compress::redact_secrets(&outcome.output));
        body["exit_code"] = serde_json::json!(outcome.exit_code);
        body["ms"] = serde_json::json!(outcome.ms);
        self.state
            .auto_capture("agent_route", None, &outcome.output)
            .await;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(
        description = "Scan a directory for security & license issues in AI-generated code using a named profile (CIS / NIST SSDF / OWASP Top 10 / ASVS / ai-default / ai-strict). Returns a ScanReport: findings with severity, file:line, fix hint, and the standards each rule maps to (CWE/OWASP/NIST/...), plus per-severity counts."
    )]
    async fn security_scan(
        &self,
        Parameters(args): Parameters<SecurityScanArgs>,
    ) -> Result<CallToolResult, McpError> {
        let profile = rtrt_security::load_profile(&args.profile)
            .map_err(|e| McpError::invalid_params(format!("security.scan profile: {e}"), None))?;
        let path = args.path.unwrap_or_else(|| ".".to_string());
        let report = rtrt_security::run(&profile, std::path::Path::new(&path))
            .map_err(|e| McpError::internal_error(format!("security.scan: {e}"), None))?;
        let body = serde_json::to_string(&report)
            .map_err(|e| McpError::internal_error(format!("security.scan serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
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
        // Honest, runtime-accurate framing: hybrid recall is only live when an
        // embedder actually got attached at startup (embeddings enabled AND
        // Ollama reachable) — otherwise every memory tool is pure BM25.
        let recall_mode = if self.state.embedder.is_some() {
            "hybrid (BM25 + vector RRF) when project embedding coverage is meaningful, else BM25"
        } else {
            "BM25 (FTS5) — no embedder attached"
        };
        let instructions = format!(
            "RTRT MCP server. Memory tools: memory_save (also opportunistically embeds new backlog when an embedder is attached) / \
                 memory_recall ({recall_mode}; payload filter always uses BM25) / \
                 memory_timeline (paginated history) / memory_profile (per-project stats) / \
                 memory_smart_search ({recall_mode}) / \
                 memory_relations (graph BFS from seed ids) / memory_export (JSONL) / \
                 memory_consolidate (archive oldest, keep most recent N) / \
                 memory_sessions (group rows by session_id, or list rows in one session) / \
                 memory_set_block / memory_get_block / memory_list_blocks (persona / human / context slots). \
                 Token tools: compress (rule rewriter) / compress_ml (token-importance) / \
                 proxy (command output filters). \
                 Code tools: repo_map (tree-sitter signatures). \
                 Project tools: templates_list / templates_scaffold. \
                 LLM tools: provider_chat (Anthropic / OpenAI / OpenAI-compatible) / agent_call (detected CLI/API agent bridge). \
                 Security tools: security_scan (profile-driven secrets / license / dependency / pattern / AI-artifact scan; profiles map to CWE/OWASP/NIST/CIS/SLSA/EU-AI-Act). \
                 Prompts: every entry in the local PromptRegistry (~/.rtrt/prompts) is exposed via prompts/list + prompts/get with handlebars argument substitution. \
                 Resources: memory://<project>/timeline lists recent rows, memory://<project>/block/<name> reads a Letta block."
        );
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(instructions)
    }

    async fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListPromptsResult, McpError> {
        let Some(registry) = self.state.prompts.as_ref() else {
            return Ok(rmcp::model::ListPromptsResult::default());
        };
        let names = registry
            .list_names()
            .map_err(|e| McpError::internal_error(format!("prompts/list: {e}"), None))?;
        let mut prompts = Vec::with_capacity(names.len());
        for name in names {
            let latest = registry
                .latest(&name)
                .map_err(|e| McpError::internal_error(format!("prompts/list latest: {e}"), None))?;
            let description = latest
                .as_ref()
                .map(|p| format!("v{} ({} chars)", p.version, p.body.chars().count()));
            prompts.push(rmcp::model::Prompt::new::<_, String>(
                name,
                description,
                None,
            ));
        }
        Ok(rmcp::model::ListPromptsResult {
            next_cursor: None,
            prompts,
            meta: None,
        })
    }

    async fn get_prompt(
        &self,
        request: rmcp::model::GetPromptRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::GetPromptResult, McpError> {
        let registry = self
            .state
            .prompts
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("prompt registry not configured", None))?;
        let prompt = registry
            .latest(&request.name)
            .map_err(|e| McpError::internal_error(format!("prompts/get: {e}"), None))?
            .ok_or_else(|| {
                McpError::invalid_params(format!("unknown prompt: {}", request.name), None)
            })?;
        let mut vars: std::collections::BTreeMap<String, String> = Default::default();
        if let Some(args) = request.arguments {
            for (k, v) in args {
                let stringified = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                vars.insert(k, stringified);
            }
        }
        let rendered = rtrt_templates::render::render_str(&prompt.body, &vars)
            .map_err(|e| McpError::internal_error(format!("prompts/get render: {e}"), None))?;
        let message =
            rmcp::model::PromptMessage::new_text(rmcp::model::PromptMessageRole::User, rendered);
        Ok(rmcp::model::GetPromptResult::new(vec![message])
            .with_description(format!("{} v{}", prompt.name, prompt.version)))
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, McpError> {
        let store = self.state.memory.lock().await;
        let projects = store
            .projects()
            .map_err(|e| McpError::internal_error(format!("resources/list: {e}"), None))?;
        let mut resources = Vec::new();
        for (project, count, _) in projects {
            let uri = format!("memory://{project}/timeline");
            let raw = rmcp::model::RawResource {
                uri,
                name: format!("{project} timeline"),
                title: Some(format!("{project} — {count} rows")),
                description: Some(format!(
                    "Newest-first memory timeline for project `{project}`."
                )),
                mime_type: Some("application/json".into()),
                size: None,
                icons: None,
                meta: None,
            };
            resources.push(rmcp::model::Annotated::new(raw, None));
            let blocks = store.list_blocks(&project).map_err(|e| {
                McpError::internal_error(format!("resources/list blocks: {e}"), None)
            })?;
            for block in blocks {
                let block_name = block
                    .kind
                    .strip_prefix("block:")
                    .unwrap_or(&block.kind)
                    .to_string();
                let uri = format!("memory://{project}/block/{block_name}");
                let raw = rmcp::model::RawResource {
                    uri,
                    name: format!("{project}/{block_name}"),
                    title: Some(format!("{project} block `{block_name}`")),
                    description: Some(format!(
                        "Letta-style memory block (project `{project}`, slot `{block_name}`)."
                    )),
                    mime_type: Some("text/plain".into()),
                    size: Some(block.body.len() as u32),
                    icons: None,
                    meta: None,
                };
                resources.push(rmcp::model::Annotated::new(raw, None));
            }
        }
        Ok(rmcp::model::ListResourcesResult {
            next_cursor: None,
            resources,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        let uri = request.uri.clone();
        let parsed = parse_memory_uri(&uri).ok_or_else(|| {
            McpError::invalid_params(format!("unsupported resource URI: {uri}"), None)
        })?;
        let store = self.state.memory.lock().await;
        let body = match parsed {
            MemoryUri::Timeline { project, limit } => {
                let rows = store
                    .recent_paged(&project, limit, 0)
                    .map_err(|e| McpError::internal_error(format!("read timeline: {e}"), None))?;
                let items: Vec<_> = rows
                    .into_iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "kind": r.kind,
                            "body": r.body,
                            "created_at": r.created_at,
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&serde_json::json!({
                    "project": project,
                    "items": items,
                }))
                .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?
            }
            MemoryUri::Block { project, name } => store
                .get_block(&project, &name)
                .map_err(|e| McpError::internal_error(format!("read block: {e}"), None))?
                .map(|b| b.body)
                .ok_or_else(|| {
                    McpError::invalid_params(format!("block not found: {project}/{name}"), None)
                })?,
        };
        let mime = match &uri {
            u if u.contains("/timeline") => "application/json",
            _ => "text/plain",
        };
        Ok(rmcp::model::ReadResourceResult::new(vec![
            rmcp::model::ResourceContents::text(body, uri).with_mime_type(mime),
        ]))
    }
}

/// Memory URI parser. Two schemes are supported:
///   `memory://<project>/timeline[?limit=N]`
///   `memory://<project>/block/<name>`
enum MemoryUri {
    Timeline { project: String, limit: usize },
    Block { project: String, name: String },
}

fn parse_memory_uri(uri: &str) -> Option<MemoryUri> {
    let rest = uri.strip_prefix("memory://")?;
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let mut parts = path.splitn(3, '/');
    let project = parts.next()?.to_string();
    let kind = parts.next()?;
    match kind {
        "timeline" => {
            let limit = query
                .and_then(|q| {
                    q.split('&')
                        .find_map(|kv| kv.strip_prefix("limit=").map(|v| v.to_string()))
                })
                .and_then(|v| v.parse().ok())
                .unwrap_or(50);
            Some(MemoryUri::Timeline { project, limit })
        }
        "block" => {
            let name = parts.next()?.to_string();
            Some(MemoryUri::Block { project, name })
        }
        _ => None,
    }
}

/// Cheap reachability probe for an `http(s)://host:port[/path]` embeddings
/// endpoint: a bare TCP connect with a short timeout. Deliberately NOT a full
/// HTTP round-trip or an actual embed call — startup shouldn't pay for that
/// just to decide whether hybrid recall is worth wiring up, and a per-call
/// embed failure falls back to BM25 regardless.
fn ollama_reachable(base_url: &str, timeout: std::time::Duration) -> bool {
    let host_port = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    if host_port.is_empty() {
        return false;
    }
    use std::net::ToSocketAddrs;
    match host_port.to_socket_addrs() {
        Ok(mut addrs) => addrs
            .next()
            .is_some_and(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()),
        Err(_) => false,
    }
}

/// Builds the embedder attached to `RtrtState`, if any. `None` when
/// embeddings are disabled in `cfg` (config/env) or the one-time reachability
/// probe can't reach the resolved Ollama endpoint — in either case every
/// memory tool stays pure BM25 with zero Ollama traffic for the rest of the
/// server's lifetime (a config edit or a newly-started Ollama takes effect on
/// the next restart, matching the dashboard's auto-embed daemon).
async fn build_embedder(cfg: &rtrt_core::Config) -> Option<Arc<dyn Embedder>> {
    if !cfg.embeddings.is_enabled() {
        tracing::info!(
            "embeddings disabled (set RTRT_EMBED_ENABLED=1 or [embeddings] enabled=true); memory_recall/memory_smart_search stay BM25-only"
        );
        return None;
    }
    let base_url = cfg
        .embeddings
        .resolved_base_url(cfg.auto_compress.base_url.as_deref());
    let reachable = {
        let base_url = base_url.clone();
        tokio::task::spawn_blocking(move || ollama_reachable(&base_url, EMBED_PROBE_TIMEOUT))
            .await
            .unwrap_or(false)
    };
    if !reachable {
        tracing::info!(
            "embeddings enabled but Ollama at {base_url} unreachable; memory_recall/memory_smart_search stay BM25-only"
        );
        return None;
    }
    let embedder = rtrt_memory::hybrid_embedder_from_config(cfg);
    tracing::info!(
        "hybrid recall ready: embedder model={} base_url={base_url}",
        embedder.model_name()
    );
    Some(Arc::new(embedder) as Arc<dyn Embedder>)
}

fn open_prompt_registry() -> Option<Arc<PromptRegistry>> {
    let root = std::env::var("RTRT_PROMPTS_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(rtrt_templates::prompts::default_dir)?;
    match PromptRegistry::open(&root) {
        Ok(r) => Some(Arc::new(r)),
        Err(e) => {
            tracing::warn!("prompt registry at {}: {e}", root.display());
            None
        }
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
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    let embedder = build_embedder(&cfg).await;
    let gateway = Arc::new(Gateway::from_env());
    let prompts = open_prompt_registry();
    let auto_capture = std::env::var("RTRT_AUTO_CAPTURE")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(true);
    let auto_redact = std::env::var("RTRT_AUTO_REDACT")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "no"))
        .unwrap_or(true);
    let dedup_window_sec = std::env::var("RTRT_AUTO_DEDUP_WINDOW_SEC")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(300);
    let default_project = std::env::var("RTRT_DEFAULT_PROJECT")
        .or_else(|_| {
            std::env::current_dir().map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "default".to_string())
            })
        })
        .unwrap_or_else(|_| "default".to_string());
    let session_id = Uuid::new_v4().to_string();
    let shared_state = Arc::new(RtrtState {
        memory: Mutex::new(memory),
        memory_path: cli.memory.clone(),
        cfg,
        embedder,
        gateway,
        prompts,
        auto_capture,
        auto_redact,
        default_project,
        session_id,
        dedup_window_sec,
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

    #[test]
    fn agent_route_capability_parses_every_label_including_agentic() {
        let cases = [
            ("code", Capability::Code),
            ("reasoning", Capability::Reasoning),
            ("vision", Capability::Vision),
            ("embed", Capability::Embed),
            ("agentic", Capability::Agentic),
            ("cheap", Capability::CheapBulk),
        ];
        for (label, expected) in cases {
            let parsed = parse_agent_route_capability(Some(label))
                .unwrap_or_else(|e| panic!("'{label}' should parse: {e:?}"));
            assert_eq!(parsed, Some(expected), "label '{label}'");
        }
        // Case/whitespace tolerant, None passthrough, unknown rejected.
        assert_eq!(
            parse_agent_route_capability(Some(" Agentic ")).unwrap(),
            Some(Capability::Agentic)
        );
        assert_eq!(parse_agent_route_capability(None).unwrap(), None);
        assert!(parse_agent_route_capability(Some("telepathy")).is_err());
    }

    #[test]
    fn ollama_reachable_returns_false_for_a_closed_port() {
        // Port 1 is a well-known reserved port almost never listened on: the
        // connection is refused outright (not merely slow), so this returns
        // fast even with a short timeout.
        assert!(!ollama_reachable(
            "http://127.0.0.1:1",
            std::time::Duration::from_millis(200)
        ));
    }

    #[test]
    fn ollama_reachable_returns_false_for_malformed_or_empty_url() {
        assert!(!ollama_reachable("", std::time::Duration::from_millis(200)));
        assert!(!ollama_reachable(
            "not a url",
            std::time::Duration::from_millis(200)
        ));
    }

    /// Deterministic, network-free embedder for exercising the hybrid-vs-BM25
    /// selection logic without a real Ollama.
    struct MockEmbedder;

    impl Embedder for MockEmbedder {
        fn dimension(&self) -> usize {
            2
        }
        fn model_name(&self) -> &str {
            "mock-test-embedder"
        }
        fn embed(&self, texts: &[&str]) -> rtrt_core::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
        }
    }

    /// Fresh on-disk temp store path — hybrid recall reopens its own
    /// connection by path (see `RtrtState::try_hybrid_recall`), so an
    /// in-memory `:memory:` db won't do: each reopen would see an empty
    /// database. Callers should remove the returned path (+ `-wal`/`-shm`
    /// sidecars) when done.
    fn temp_store_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rtrt-mcp-test-{tag}-{}.sqlite", Uuid::new_v4()));
        p
    }

    fn cleanup_store(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(PathBuf::from(format!("{}-wal", path.display())));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}-shm", path.display())));
    }

    fn test_state(
        path: PathBuf,
        cfg: rtrt_core::Config,
        embedder: Option<Arc<dyn Embedder>>,
    ) -> RtrtState {
        let memory = MemoryStore::open(&path).expect("open temp store");
        RtrtState {
            memory: Mutex::new(memory),
            memory_path: path,
            cfg,
            embedder,
            gateway: Arc::new(Gateway::new()),
            prompts: None,
            auto_capture: false,
            auto_redact: true,
            default_project: "test".to_string(),
            session_id: "test-session".to_string(),
            dedup_window_sec: 0,
        }
    }

    #[tokio::test]
    async fn try_hybrid_recall_returns_none_without_an_embedder() {
        let path = temp_store_path("no-embedder");
        let mut cfg = rtrt_core::Config::default();
        cfg.embeddings.enabled = true;
        let state = test_state(path.clone(), cfg, None);

        // No embedder attached at startup -> None immediately, regardless of
        // config or store state (zero Ollama traffic for the BM25-only user).
        assert!(state.try_hybrid_recall("p", "rust", 5).await.is_none());
        cleanup_store(&path);
    }

    #[tokio::test]
    async fn try_hybrid_recall_falls_back_when_coverage_is_too_low() {
        let path = temp_store_path("low-coverage");
        let mut cfg = rtrt_core::Config::default();
        cfg.embeddings.enabled = true;
        let state = test_state(path.clone(), cfg, Some(Arc::new(MockEmbedder)));

        {
            let store = state.memory.lock().await;
            store.save("p", "note", "rust cargo workspace").unwrap();
            store.save("p", "note", "python pip dependencies").unwrap();
            // 0 of 2 rows embedded: below the 50% coverage floor.
        }

        assert!(state.try_hybrid_recall("p", "rust", 5).await.is_none());
        cleanup_store(&path);
    }

    #[tokio::test]
    async fn try_hybrid_recall_returns_hits_once_gates_pass() {
        let path = temp_store_path("ready");
        let mut cfg = rtrt_core::Config::default();
        cfg.embeddings.enabled = true;
        let state = test_state(path.clone(), cfg, Some(Arc::new(MockEmbedder)));

        {
            let store = state.memory.lock().await;
            let a = store.save("p", "note", "rust cargo workspace").unwrap();
            store.save("p", "note", "python pip dependencies").unwrap();
            // Embed exactly half (1/2): embedded*2 >= total -> ready.
            store
                .store_embedding(a, MockEmbedder.model_name(), &[3.0, 1.0])
                .unwrap();
        }

        let hits = state
            .try_hybrid_recall("p", "rust", 5)
            .await
            .expect("gates should pass and hybrid recall should succeed");
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|h| h.body.contains("rust")), "{hits:?}");
        cleanup_store(&path);
    }

    #[tokio::test]
    async fn memory_save_triggers_opportunistic_sweep_only_with_an_embedder() {
        let path = temp_store_path("sweep");
        let cfg = rtrt_core::Config::default();
        let state = test_state(path.clone(), cfg, Some(Arc::new(MockEmbedder)));

        {
            let store = state.memory.lock().await;
            store.save("p", "note", "alpha").unwrap();
            store.save("p", "note", "beta").unwrap();
        }
        assert_eq!(
            state.memory.lock().await.unembedded_count().unwrap(),
            2,
            "nothing embedded yet"
        );

        state.spawn_opportunistic_embed_sweep();
        // The sweep runs on a detached blocking task; give it a moment to
        // finish rather than asserting on an unpredictable race.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let remaining = state.memory.lock().await.unembedded_count().unwrap();
        // Backlog of 2 -> batch size sqrt(2).ceil() == 2, so the sweep should
        // have cleared the whole backlog in one hop.
        assert_eq!(remaining, 0);
        cleanup_store(&path);
    }
}
