//! rtrt-mcp — MCP server exposing the RTRT toolkit's surfaces as tools.
//!
//! Two transports:
//! - **stdio** (default) — standard MCP framing for local agent integrations.
//! - **http** — Streamable HTTP (MCP 2025-06-18) served by `rmcp`'s
//!   `StreamableHttpService` behind an axum router. Defaults to loopback for
//!   DNS-rebinding safety; the bind address is configurable.

use std::path::{Component, Path, PathBuf};
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
use rtrt_core::{Capability, CompressionLevel, DetectedTool, ProjectIdentity, pool_from_model};
use rtrt_memory::{Embedder, MemoryStore};
use rtrt_providers::{
    ChatMessage, ChatRequest, DEFAULT_TIMEOUT_SECS, Gateway, InvocationContext, InvokeOptions,
    Mode as InvokeMode, Prefer, Role, RouteRequest, UsageSnapshot, invoke_agent,
    invoke_with_failover_context, select_route,
};
use rtrt_templates::PromptRegistry;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "rtrt-mcp", version, about = "RTRT MCP server (stdio + http)", long_about = None)]
struct Cli {
    /// Legacy/admin SQLite path. Rejected unless `--admin` is also present.
    #[arg(long, env = "RTRT_MEMORY_PATH")]
    memory: Option<PathBuf>,
    /// Enable legacy/admin stdio mode. Required for `--memory`; unavailable over HTTP.
    #[arg(long)]
    admin: bool,
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
    /// Allowed browser Origins (comma-separated) for `--transport http`.
    /// Empty rejects every request that carries an `Origin` header at all;
    /// non-empty admits exactly the listed origins per RFC 6454.
    #[arg(long, env = "RTRT_MCP_ALLOWED_ORIGINS", value_delimiter = ',')]
    allowed_origins: Vec<String>,
    /// Stdio-only strict profile exposing only `permission_prompt`.
    #[arg(long)]
    permission_only: bool,
    /// Allow HTTP clients to invoke process-execution tools. Denied by default.
    #[arg(long)]
    http_allow_process_execution: bool,
    /// Allow HTTP clients to invoke network tools and enable startup embedder
    /// probing. Denied by default.
    #[arg(long)]
    http_allow_network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeProfile {
    PermissionOnly,
    StdioFull,
    AdminStdio,
    Http {
        process_execution: bool,
        network: bool,
    },
}

impl RuntimeProfile {
    fn from_cli(cli: &Cli) -> Result<Self> {
        if cli.memory.is_some() && !cli.admin {
            anyhow::bail!("--memory/RTRT_MEMORY_PATH requires explicit --admin mode");
        }
        if cli.admin && cli.memory.is_none() {
            anyhow::bail!("--admin requires --memory or RTRT_MEMORY_PATH");
        }
        if cli.admin && (cli.permission_only || matches!(cli.transport, Transport::Http)) {
            anyhow::bail!(
                "--admin is legacy stdio-only and cannot be combined with --permission-only or --transport http"
            );
        }
        match cli.transport {
            Transport::Stdio if cli.permission_only => {
                if cli.http_allow_process_execution || cli.http_allow_network {
                    anyhow::bail!(
                        "--http-allow-process-execution and --http-allow-network require --transport http"
                    );
                }
                Ok(Self::PermissionOnly)
            }
            Transport::Stdio if cli.admin => Ok(Self::AdminStdio),
            Transport::Stdio => {
                if cli.http_allow_process_execution || cli.http_allow_network {
                    anyhow::bail!(
                        "--http-allow-process-execution and --http-allow-network require --transport http"
                    );
                }
                Ok(Self::StdioFull)
            }
            Transport::Http if cli.permission_only => {
                anyhow::bail!("--permission-only is valid only with --transport stdio")
            }
            Transport::Http => Ok(Self::Http {
                process_execution: cli.http_allow_process_execution,
                network: cli.http_allow_network,
            }),
        }
    }

    fn allows_prompts_resources(self) -> bool {
        !matches!(self, Self::PermissionOnly)
    }

    fn allows_startup_network(self) -> bool {
        matches!(
            self,
            Self::StdioFull | Self::AdminStdio | Self::Http { network: true, .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolCapabilities {
    process_execution: bool,
    provider_network: bool,
}

impl ToolCapabilities {
    const SAFE: Self = Self {
        process_execution: false,
        provider_network: false,
    };
    const PROVIDER_NETWORK: Self = Self {
        process_execution: false,
        provider_network: true,
    };
    const PROVIDER_EXECUTION: Self = Self {
        process_execution: true,
        provider_network: true,
    };

    fn allowed(self, process_execution: bool, network: bool) -> bool {
        (!self.process_execution || process_execution) && (!self.provider_network || network)
    }
}

/// Explicit HTTP capability inventory. Unknown tools fail closed until their
/// process/provider-network behavior is reviewed and classified here.
fn tool_capabilities(name: &str) -> Option<ToolCapabilities> {
    Some(match name {
        "agent_call" | "agent_route" => ToolCapabilities::PROVIDER_EXECUTION,
        "provider_chat" => ToolCapabilities::PROVIDER_NETWORK,
        "permission_prompt"
        | "compress"
        | "compress_ml"
        | "proxy"
        | "memory_save"
        | "memory_recall"
        | "memory_timeline"
        | "memory_profile"
        | "memory_relations"
        | "memory_smart_search"
        | "memory_consolidate"
        | "memory_sessions"
        | "memory_export"
        | "memory_set_block"
        | "memory_get_block"
        | "memory_list_blocks"
        | "repo_map"
        | "templates_list"
        | "templates_scaffold"
        | "security_scan" => ToolCapabilities::SAFE,
        _ => return None,
    })
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

const PERMISSION_BROKER_PATH: &str = "/rtrt/permission/v1";
const PERMISSION_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PERMISSION_MAX_BODY: usize = 32_768;
const PERMISSION_MAX_RESPONSE: usize = 16_384;
const PERMISSION_MAX_DEPTH: usize = 32;
const PERMISSION_MAX_STRING: usize = 8_192;
const PERMISSION_MAX_NAME: usize = 256;
const PERMISSION_MAX_ID: usize = 1_024;

struct RtrtState {
    profile: RuntimeProfile,
    memory: Option<Mutex<MemoryStore>>,
    /// Path the primary `memory` store was opened from. Kept so background
    /// work (hybrid recall's bounded worker, the opportunistic embed sweep)
    /// can open its OWN connection instead of contending with `memory`'s lock
    /// for the duration of a possibly-slow Ollama call.
    memory_binding: Option<MemoryBinding>,
    project: Option<ProjectIdentity>,
    /// Config snapshot taken at startup (mirrors the dashboard's auto-embed
    /// daemon, which also reads config once rather than per-cycle). A config
    /// edit while the server is running takes effect on the next restart.
    cfg: rtrt_core::Config,
    /// Attached iff `[embeddings] enabled` (config/env) AND the startup probe
    /// found Ollama reachable. `None` keeps every memory tool pure BM25 with
    /// zero Ollama traffic.
    embedder: Option<Arc<dyn Embedder>>,
    gateway: Option<Arc<Gateway>>,
    prompts: Option<Arc<PromptRegistry>>,
    auto_capture: bool,
    auto_redact: bool,
    session_id: String,
    dedup_window_sec: i64,
    /// Canonical project directories that filesystem-capable tools may access.
    authorized_roots: Vec<PathBuf>,
}

#[derive(Clone)]
enum MemoryBinding {
    Project(ProjectIdentity),
    AdminPath(PathBuf),
}

fn canonical_authorized_root(path: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(path)?;
    if root.parent().is_none() || !root.is_dir() {
        anyhow::bail!(
            "authorized project root must be a non-root directory: {}",
            path.display()
        );
    }
    Ok(root)
}

fn authorize_project_path(
    roots: &[PathBuf],
    requested: &Path,
    must_exist: bool,
) -> std::result::Result<PathBuf, String> {
    let [root] = roots else {
        return Err(match roots.len() {
            0 => "no authorized project root configured".to_string(),
            count => {
                format!("exactly one authorized project root is supported; configured {count}")
            }
        });
    };
    if requested.as_os_str().is_empty()
        || requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("invalid project path: {}", requested.display()));
    }
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    if candidate.parent().is_none() {
        return Err("filesystem root is not an authorized project path".to_string());
    }
    if !candidate.starts_with(root) {
        return Err(format!(
            "path is outside authorized project root: {}",
            requested.display()
        ));
    }
    let authorized_root = root;

    let relative = candidate.strip_prefix(authorized_root).map_err(|_| {
        format!(
            "path is outside authorized project root: {}",
            requested.display()
        )
    })?;
    let mut current = authorized_root.clone();
    let mut nonexistent = false;
    for component in relative.components() {
        if !matches!(component, Component::Normal(_) | Component::CurDir) {
            return Err(format!("invalid project path: {}", requested.display()));
        }
        current.push(component.as_os_str());
        if nonexistent {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "symlink project path is not allowed: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => nonexistent = true,
            Err(error) => {
                return Err(format!(
                    "cannot inspect project path {}: {error}",
                    current.display()
                ));
            }
        }
    }

    let mut existing = candidate.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| format!("invalid project path: {}", requested.display()))?;
        suffix.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| format!("invalid project path: {}", requested.display()))?;
    }
    let mut resolved = std::fs::canonicalize(existing).map_err(|error| {
        format!(
            "cannot resolve project path {}: {error}",
            requested.display()
        )
    })?;
    if !resolved.starts_with(authorized_root) {
        return Err(format!(
            "project path escapes authorized root: {}",
            requested.display()
        ));
    }
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    if must_exist && (!resolved.exists() || !resolved.is_dir()) {
        return Err(format!(
            "project directory not found: {}",
            requested.display()
        ));
    }
    Ok(resolved)
}

impl RtrtState {
    fn project_identity(&self) -> Result<&ProjectIdentity, McpError> {
        self.project
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("project memory is disabled", None))
    }

    fn pinned_project(&self, requested: Option<&str>) -> Result<String, McpError> {
        let identity = self.project_identity()?;
        let requested = requested.unwrap_or_default().trim();
        if requested.is_empty() || requested == identity.slug() || requested == identity.label() {
            Ok(identity.slug().to_string())
        } else {
            Err(McpError::invalid_params(
                format!(
                    "foreign project rejected: requested '{requested}', server is pinned to '{}'",
                    identity.slug()
                ),
                None,
            ))
        }
    }

    fn memory(&self) -> Result<&Mutex<MemoryStore>, McpError> {
        self.memory
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("memory is disabled", None))
    }

    fn open_background_store(binding: &MemoryBinding) -> Option<MemoryStore> {
        match binding {
            MemoryBinding::Project(identity) => MemoryStore::open_project(identity).ok(),
            MemoryBinding::AdminPath(path) => MemoryStore::open(path).ok(),
        }
    }

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
        let binding = self.memory_binding.clone()?;
        let project = project.to_string();
        let query = query.to_string();
        let task =
            tokio::task::spawn_blocking(move || -> Option<Vec<rtrt_memory::MemoryRecord>> {
                let store = Self::open_background_store(&binding)?;
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
        let Some(binding) = self.memory_binding.clone() else {
            return;
        };
        tokio::task::spawn_blocking(move || match Self::open_background_store(&binding) {
            Some(store) => {
                let embedded = store.opportunistic_embed_sweep(embedder.as_ref());
                if embedded > 0 {
                    tracing::info!(embedded, "mcp opportunistic embed sweep");
                }
            }
            None => tracing::warn!("mcp opportunistic embed sweep: open store failed"),
        });
    }

    /// Best-effort capture mirroring the dashboard pipeline:
    /// `redact_secrets` → SHA-256 dedup → save → tag(session, sha).
    /// Skipped when `auto_capture` is off. Errors are swallowed so a memory
    /// hiccup never breaks the tool call that triggered it.
    async fn auto_capture(
        &self,
        kind: &str,
        project_override: Option<&str>,
        body: &str,
    ) -> Result<(), McpError> {
        let project = self.pinned_project(project_override)?;
        if !self.auto_capture {
            return Ok(());
        }
        let filtered = if self.auto_redact {
            rtrt_compress::redact_secrets(body)
        } else {
            body.to_string()
        };
        let sha = MemoryStore::body_sha(&filtered);
        let store = self.memory()?.lock().await;
        if self.dedup_window_sec > 0
            && let Ok(Some(seen_at)) = store.body_seen_at(&project, &sha)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if now.saturating_sub(seen_at) < self.dedup_window_sec {
                return Ok(());
            }
        }
        let Ok(id) = store.save(&project, kind, &filtered) else {
            return Ok(());
        };
        let _ = store.tag_row(id, Some(&self.session_id), Some(&sha));
        Ok(())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompressArgs {
    /// Text to compress.
    text: String,
    /// One of `lite`, `full`, `ultra`. Defaults to `full`.
    #[serde(default)]
    level: Option<String>,
    /// Project used for auto-capture. Required for shared HTTP servers.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySaveArgs {
    #[serde(default)]
    project: Option<String>,
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
    /// Project used for auto-capture. Required for shared HTTP servers.
    #[serde(default)]
    project: Option<String>,
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
    /// Project used for auto-capture. Required for shared HTTP servers.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryRecallArgs {
    #[serde(default)]
    project: Option<String>,
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
    #[serde(default)]
    project: Option<String>,
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
    #[serde(default)]
    project: Option<String>,
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
    #[serde(default)]
    project: Option<String>,
    seed_ids: Vec<i64>,
    #[serde(default = "default_depth")]
    depth: u32,
}

fn default_depth() -> u32 {
    2
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySmartSearchArgs {
    #[serde(default)]
    project: Option<String>,
    query: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryConsolidateArgs {
    #[serde(default)]
    project: Option<String>,
    #[serde(default = "default_keep")]
    keep: u32,
}

fn default_keep() -> u32 {
    20
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySessionsArgs {
    #[serde(default)]
    project: Option<String>,
    /// Optional `session_id`. When set, returns the rows in that session
    /// instead of the per-session summary list.
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default = "default_timeline_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemorySetBlockArgs {
    #[serde(default)]
    project: Option<String>,
    /// Block name. Typical: `persona`, `human`, `context`. Free-form slug.
    name: String,
    body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryGetBlockArgs {
    #[serde(default)]
    project: Option<String>,
    name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MemoryListBlocksArgs {
    #[serde(default)]
    project: Option<String>,
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
    /// Project used for auto-capture. Required for shared HTTP servers.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProviderChatMessage {
    role: String,
    content: String,
}

/// Parent execution identity injected by the RTRT OpenCode plugin. Human and
/// model callers normally omit these fields; absence preserves legacy calls.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
struct InvocationContextArgs {
    #[serde(default)]
    invocation_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted for wire compatibility; never trusted for attribution
    parent_project: Option<String>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    parent_call_id: Option<String>,
    #[serde(default)]
    caller_agent: Option<String>,
    #[serde(default)]
    parent_cwd: Option<String>,
    #[serde(default)]
    parent_worktree: Option<String>,
}

impl InvocationContextArgs {
    fn resolve(&self, project: Option<&str>) -> Option<InvocationContext> {
        let invocation_id = self
            .invocation_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())?
            .to_string();
        Some(InvocationContext {
            invocation_id,
            // Caller-supplied provenance is descriptive only. Project
            // attribution comes exclusively from the server-pinned value.
            parent_project: project
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string),
            parent_session_id: self.parent_session_id.clone(),
            parent_call_id: self.parent_call_id.clone(),
            caller_agent: self.caller_agent.clone(),
            parent_cwd: self.parent_cwd.clone(),
            parent_worktree: self.parent_worktree.clone(),
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentCallArgs {
    target: String,
    prompt: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Project used for auto-capture. Required for shared HTTP servers.
    #[serde(default)]
    project: Option<String>,
    #[serde(flatten)]
    invocation: InvocationContextArgs,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct AgentRouteArgs {
    prompt: String,
    /// Ranking *strategy*: `cheapest` (default), `local`, or `quality`. This is
    /// not a provider name — pin a provider with `target`.
    #[serde(default)]
    prefer: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    model: Option<String>,
    /// Pin a detected target (provider) by name, e.g. `opencode`. The target
    /// resolves to its roomiest pool unless `model` names one; without
    /// `failover` it is the only candidate.
    #[serde(default)]
    target: Option<String>,
    /// Invocation mode for the selected target: `cli`, `api`, or `auto`
    /// (default — pick whichever the target supports).
    #[serde(default)]
    mode: Option<String>,
    /// Keep the ranked fallbacks behind the pick — sibling pools of the same
    /// target first — and fall over to them on a recoverable failure. Defaults
    /// to `false`, which routes to exactly one candidate as before.
    #[serde(default)]
    failover: Option<bool>,
    /// Project used for auto-capture. Required for shared HTTP servers.
    #[serde(default)]
    project: Option<String>,
    #[serde(flatten)]
    invocation: InvocationContextArgs,
}

/// The routing request behind one `agent_route` call.
///
/// Split out of the handler so the argument → [`RouteRequest`] mapping is
/// testable without detection or a live provider. Every field absent from the
/// arguments keeps the pre-existing default, so a legacy-shaped call builds the
/// exact request it always did.
fn agent_route_request(args: &AgentRouteArgs) -> Result<RouteRequest, McpError> {
    Ok(RouteRequest {
        capability: parse_agent_route_capability(args.capability.as_deref())?,
        prefer: parse_agent_route_prefer(args.prefer.as_deref())?,
        target: args.target.clone(),
        model: args.model.clone(),
        mode: parse_agent_route_mode(args.mode.as_deref())?,
        failover: args.failover.unwrap_or(false),
    })
}

/// The accepted `prefer` strategies, listed in the error the way the capability
/// parser lists its own — a rejected value must always say what *is* accepted.
const AGENT_ROUTE_PREFER_VALUES: &str = "cheapest, local, or quality";

/// A detected routing identity a caller might mistake for a `prefer` strategy.
///
/// `prefer` decides *how* candidates are ranked; naming a provider or one of its
/// upstream pools is what `target` (plus `model`, for a pool) is for. Telling
/// the caller which of the two they actually named turns a dead-end
/// "unknown prefer" into a fix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DetectedRoute {
    /// A detected target name, e.g. `opencode`.
    Target { target: String },
    /// A pool reachable through a detected target, named by a model prefix:
    /// `opencode-go` from `opencode-go/glm-5.2`.
    Pool {
        target: String,
        pool: String,
        model: String,
    },
}

impl DetectedRoute {
    /// How to pass this identity correctly.
    fn hint(&self) -> String {
        match self {
            Self::Target { target } => format!(
                "'{target}' is a detected target, not a strategy: pass target=\"{target}\" instead"
            ),
            Self::Pool {
                target,
                pool,
                model,
            } => format!(
                "'{pool}' is a pool of the detected target '{target}', not a strategy: pass \
                 target=\"{target}\" (with model=\"{model}\" to pin that pool) instead"
            ),
        }
    }
}

/// Resolve a rejected `prefer` value against the detected tools: an exact target
/// name first, then any pool named by one of their model prefixes.
fn detected_route_in(tools: &[DetectedTool], value: &str) -> Option<DetectedRoute> {
    let needle = value.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    if let Some(tool) = tools
        .iter()
        .find(|tool| tool.name.to_ascii_lowercase() == needle)
    {
        return Some(DetectedRoute::Target {
            target: tool.name.clone(),
        });
    }
    tools.iter().find_map(|tool| {
        tool.models.iter().find_map(|model| {
            let pool = pool_from_model(model)?;
            (pool == needle).then(|| DetectedRoute::Pool {
                target: tool.name.clone(),
                pool: pool.clone(),
                model: model.clone(),
            })
        })
    })
}

/// Detection is only run on the error path — a valid `prefer` never probes the
/// host.
fn detected_route_named(value: &str) -> Option<DetectedRoute> {
    let cfg = rtrt_core::Config::load_effective_for_cwd();
    let tools = rtrt_core::detect_tools_with_config(cfg);
    detected_route_in(&tools, value)
}

fn parse_agent_route_prefer(value: Option<&str>) -> Result<Prefer, McpError> {
    parse_agent_route_prefer_with(value, detected_route_named)
}

/// The parse itself, with the "did you mean a target?" lookup injected so tests
/// can exercise the message without detecting anything on the host.
fn parse_agent_route_prefer_with(
    value: Option<&str>,
    detected: impl FnOnce(&str) -> Option<DetectedRoute>,
) -> Result<Prefer, McpError> {
    let Some(value) = value else {
        return Ok(Prefer::Cheapest);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "cheapest" => Ok(Prefer::Cheapest),
        "local" => Ok(Prefer::Local),
        "quality" => Ok(Prefer::Quality),
        other => Err(McpError::invalid_params(
            unknown_prefer_message(other, detected(other)),
            None,
        )),
    }
}

fn unknown_prefer_message(value: &str, detected: Option<DetectedRoute>) -> String {
    let base = format!(
        "agent_route prefer: unknown prefer '{value}' \
         (expected {AGENT_ROUTE_PREFER_VALUES})"
    );
    match detected {
        Some(route) => format!("{base}. {}", route.hint()),
        None => base,
    }
}

/// `cli` / `api` / `auto`, or `None` when the caller named no mode — which
/// leaves the router's own mode resolution in charge, exactly as before this
/// argument existed.
fn parse_agent_route_mode(value: Option<&str>) -> Result<Option<InvokeMode>, McpError> {
    value
        .map(|value| InvokeMode::parse_label(&value.trim().to_ascii_lowercase()))
        .transpose()
        .map_err(|e| McpError::invalid_params(format!("agent_route mode: {e}"), None))
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

/// Claude Code's permission-prompt tool contract. `input` deliberately stays a
/// JSON value so a non-object can be converted to a safe denial rather than an
/// MCP argument-deserialization error.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PermissionPromptArgs {
    tool_name: String,
    #[schemars(schema_with = "permission_input_schema")]
    input: serde_json::Value,
    #[serde(default)]
    tool_use_id: Option<String>,
}

fn permission_input_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    <std::collections::BTreeMap<String, serde_json::Value> as schemars::JsonSchema>::json_schema(
        generator,
    )
}

#[derive(Serialize)]
struct PermissionBrokerRequest<'a> {
    version: u8,
    request_id: &'a str,
    broker_nonce: &'a str,
    invocation_id: &'a str,
    parent_session_id: &'a str,
    parent_call_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_session_id: Option<&'a str>,
    tool_name: &'a str,
    input: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionBrokerResponse {
    version: u8,
    request_id: String,
    decision: String,
}

#[derive(Clone, Copy)]
enum PermissionFailure {
    Configuration,
    Request,
    Unavailable,
    Response,
    Rejected,
}

impl PermissionFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Configuration => "Permission broker is not configured.",
            Self::Request => "Permission request is invalid.",
            Self::Unavailable => "Permission broker is unavailable.",
            Self::Response => "Permission broker returned an invalid response.",
            Self::Rejected => "Permission denied by broker.",
        }
    }
}

fn permission_result(result: Result<bool, PermissionFailure>) -> CallToolResult {
    let body = match result {
        Ok(true) => serde_json::json!({ "behavior": "allow" }),
        Ok(false) => serde_json::json!({
            "behavior": "deny",
            "message": PermissionFailure::Rejected.message(),
        }),
        Err(error) => serde_json::json!({
            "behavior": "deny",
            "message": error.message(),
        }),
    };
    CallToolResult::success(vec![Content::text(body.to_string())])
}

fn parse_permission_broker_url(value: &str) -> Option<u16> {
    let authority = value
        .strip_prefix("http://127.0.0.1:")?
        .strip_suffix(PERMISSION_BROKER_PATH)?;
    if authority.is_empty() || !authority.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    authority.parse::<u16>().ok().filter(|port| *port != 0)
}

fn bounded_env(name: &str, max: usize) -> Result<String, PermissionFailure> {
    let value = std::env::var(name).map_err(|_| PermissionFailure::Configuration)?;
    if value.is_empty()
        || value.len() > max
        || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
    {
        return Err(PermissionFailure::Configuration);
    }
    Ok(value)
}

fn valid_permission_input(value: &serde_json::Value) -> bool {
    fn visit(value: &serde_json::Value, depth: usize) -> bool {
        if depth > PERMISSION_MAX_DEPTH {
            return false;
        }
        match value {
            serde_json::Value::String(value) => value.len() <= PERMISSION_MAX_STRING,
            serde_json::Value::Array(values) => values.iter().all(|value| visit(value, depth + 1)),
            serde_json::Value::Object(values) => values
                .iter()
                .all(|(key, value)| key.len() <= PERMISSION_MAX_STRING && visit(value, depth + 1)),
            _ => true,
        }
    }
    value.is_object() && visit(value, 1)
}

fn parse_permission_response(raw: &[u8], request_id: &str) -> Result<bool, PermissionFailure> {
    let separator = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(PermissionFailure::Response)?;
    let headers =
        std::str::from_utf8(&raw[..separator]).map_err(|_| PermissionFailure::Response)?;
    let mut lines = headers.split("\r\n");
    let status = lines.next().ok_or(PermissionFailure::Response)?;
    let mut status_parts = status.splitn(3, ' ');
    if status_parts.next() != Some("HTTP/1.1") || status_parts.next() != Some("200") {
        return Err(PermissionFailure::Response);
    }
    if lines.any(|line| line.is_empty() || !line.contains(':')) {
        return Err(PermissionFailure::Response);
    }
    let response: PermissionBrokerResponse =
        serde_json::from_slice(&raw[separator + 4..]).map_err(|_| PermissionFailure::Response)?;
    if response.version != 1 || response.request_id != request_id {
        return Err(PermissionFailure::Response);
    }
    match response.decision.as_str() {
        "once" | "always" => Ok(true),
        "reject" => Ok(false),
        _ => Err(PermissionFailure::Response),
    }
}

async fn request_permission(args: &PermissionPromptArgs) -> Result<bool, PermissionFailure> {
    if args.tool_name.trim().is_empty()
        || args.tool_name.len() > PERMISSION_MAX_NAME
        || args
            .tool_use_id
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > PERMISSION_MAX_ID)
        || !valid_permission_input(&args.input)
    {
        return Err(PermissionFailure::Request);
    }

    let url = bounded_env("RTRT_PERMISSION_BROKER_URL", 256)?;
    let port = parse_permission_broker_url(&url).ok_or(PermissionFailure::Configuration)?;
    let token = bounded_env("RTRT_PERMISSION_BROKER_TOKEN", PERMISSION_MAX_STRING)?;
    let nonce = bounded_env("RTRT_PERMISSION_BROKER_NONCE", PERMISSION_MAX_ID)?;
    let invocation_id = bounded_env("RTRT_INVOCATION_ID", PERMISSION_MAX_ID)?;
    let parent_session_id = bounded_env("RTRT_PARENT_SESSION_ID", PERMISSION_MAX_ID)?;
    let parent_call_id = bounded_env("RTRT_PARENT_CALL_ID", PERMISSION_MAX_ID)?;
    let child_session_id = match std::env::var("RTRT_CHILD_SESSION_ID") {
        Ok(value)
            if !value.is_empty()
                && value.len() <= PERMISSION_MAX_ID
                && !value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) =>
        {
            Some(value)
        }
        Ok(_) => return Err(PermissionFailure::Configuration),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(PermissionFailure::Configuration),
    };
    let request_id = Uuid::new_v4().to_string();
    let body = serde_json::to_vec(&PermissionBrokerRequest {
        version: 1,
        request_id: &request_id,
        broker_nonce: &nonce,
        invocation_id: &invocation_id,
        parent_session_id: &parent_session_id,
        parent_call_id: &parent_call_id,
        child_session_id: child_session_id.as_deref(),
        tool_name: &args.tool_name,
        input: &args.input,
        tool_use_id: args.tool_use_id.as_deref(),
    })
    .map_err(|_| PermissionFailure::Request)?;
    if body.len() > PERMISSION_MAX_BODY {
        return Err(PermissionFailure::Request);
    }

    let mut stream = tokio::time::timeout(
        PERMISSION_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .map_err(|_| PermissionFailure::Unavailable)?
    .map_err(|_| PermissionFailure::Unavailable)?;
    let headers = format!(
        "POST {PERMISSION_BROKER_PATH} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nX-RTRT-Broker-Nonce: {nonce}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .map_err(|_| PermissionFailure::Unavailable)?;
    stream
        .write_all(&body)
        .await
        .map_err(|_| PermissionFailure::Unavailable)?;

    let mut response = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|_| PermissionFailure::Unavailable)?;
        if read == 0 {
            break;
        }
        if response.len().saturating_add(read) > PERMISSION_MAX_RESPONSE {
            return Err(PermissionFailure::Response);
        }
        response.extend_from_slice(&chunk[..read]);
    }
    parse_permission_response(&response, &request_id)
}

#[tool_router]
impl RtrtMcp {
    /// Build from an already-shared state — same state is reused across stdio
    /// and HTTP transports so every session shares one SQLite handle + one
    /// gateway.
    pub fn with_state(state: Arc<RtrtState>) -> Self {
        let mut tool_router = Self::tool_router();
        match state.profile {
            RuntimeProfile::PermissionOnly => {
                let names: Vec<_> = tool_router
                    .list_all()
                    .into_iter()
                    .map(|tool| tool.name)
                    .filter(|name| name.as_ref() != "permission_prompt")
                    .collect();
                for name in names {
                    tool_router.disable_route(name);
                }
            }
            RuntimeProfile::Http {
                process_execution,
                network,
            } => {
                let names: Vec<_> = tool_router
                    .list_all()
                    .into_iter()
                    .map(|tool| tool.name)
                    .filter(|name| {
                        tool_capabilities(name.as_ref()).is_none_or(|capabilities| {
                            !capabilities.allowed(process_execution, network)
                        })
                    })
                    .collect();
                for name in names {
                    tool_router.disable_route(name);
                }
            }
            RuntimeProfile::StdioFull | RuntimeProfile::AdminStdio => {}
        }
        Self { tool_router, state }
    }

    #[tool(
        description = "Request a Claude tool-use permission decision from the trusted local RTRT permission broker. Returns Claude decision JSON and defaults to deny."
    )]
    async fn permission_prompt(
        &self,
        Parameters(args): Parameters<PermissionPromptArgs>,
    ) -> Result<CallToolResult, McpError> {
        // This security-sensitive bridge intentionally bypasses auto_capture:
        // neither Claude's raw tool input nor broker traffic belongs in memory.
        Ok(permission_result(request_permission(&args).await))
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
        self.state
            .auto_capture("compress", args.project.as_deref(), &out)
            .await?;
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
        self.state
            .auto_capture("compress_ml", args.project.as_deref(), &out)
            .await?;
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
        self.state
            .auto_capture("proxy", args.project.as_deref(), &out)
            .await?;
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }

    #[tool(description = "Save a memory record to the SQLite store. Returns the new id.")]
    async fn memory_save(
        &self,
        Parameters(args): Parameters<MemorySaveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let project = self.state.pinned_project(args.project.as_deref())?;
        let id = {
            let store = self.state.memory()?.lock().await;
            store
                .save(&project, &args.kind, &args.body)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        if let Some(spec) = args.filter.as_deref().filter(|s| !s.is_empty()) {
            let filter = rtrt_memory::PayloadFilter::parse(spec).map_err(|e| {
                McpError::invalid_params(format!("memory.recall filter: {e}"), None)
            })?;
            let store = self.state.memory()?.lock().await;
            let hits = store
                .recall_bm25_with_filter(&project, &args.query, args.limit as usize, &filter)
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
            .try_hybrid_recall(&project, &args.query, args.limit as usize)
            .await
        {
            Some(hits) => hits,
            None => {
                let store = self.state.memory()?.lock().await;
                store
                    .recall_bm25(&project, &args.query, args.limit as usize)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let items = store
            .recent_paged(&project, args.limit as usize, args.offset as usize)
            .map_err(|e| McpError::internal_error(format!("memory.timeline: {e}"), None))?;
        let total = store
            .count_by_project(&project)
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

    #[tool(description = "Project intelligence for the server-pinned project.")]
    async fn memory_profile(
        &self,
        Parameters(args): Parameters<MemoryProjectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let count = store
            .count_by_project(&project)
            .map_err(|e| McpError::internal_error(format!("memory.profile: {e}"), None))?;
        let latest = store
            .recent(&project, 1)
            .map_err(|e| McpError::internal_error(format!("memory.profile: {e}"), None))?
            .first()
            .map_or(0, |row| row.created_at);
        let row = serde_json::json!({ "project": project, "count": count, "latest_ts": latest });
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        for id in &args.seed_ids {
            let row = store
                .get_row(*id)
                .map_err(|e| McpError::internal_error(format!("memory.relations: {e}"), None))?;
            if row.as_ref().is_none_or(|row| row.project != project) {
                return Err(McpError::invalid_params(
                    format!("memory id {id} does not belong to pinned project"),
                    None,
                ));
            }
        }
        // Project scoping happens inside the traversal (foreign rows never
        // act as bridges) and the walk is visit-capped in the store.
        let items = store
            .recall_via_graph_scoped(&args.seed_ids, args.depth, Some(&project))
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let hits = match self
            .state
            .try_hybrid_recall(&project, &args.query, args.limit as usize)
            .await
        {
            Some(hits) => hits,
            None => {
                let store = self.state.memory()?.lock().await;
                store
                    .recall_bm25(&project, &args.query, args.limit as usize)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let mut buf: Vec<u8> = Vec::new();
        store
            .export_jsonl(&project, &mut buf)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let (removed, digest_id) = store
            .archive_overflow_no_llm(&project, args.keep as usize)
            .map_err(|e| McpError::internal_error(format!("memory.consolidate: {e}"), None))?;
        let after = store
            .count_by_project(&project)
            .map_err(|e| McpError::internal_error(format!("memory.consolidate: {e}"), None))?;
        let body = serde_json::json!({
            "project": project,
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let body = if let Some(sid) = args.session_id.as_deref() {
            let rows = store
                .session_records(&project, sid, args.limit as usize)
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
                "project": project,
                "session_id": sid,
                "items": items,
            })
        } else {
            let summaries = store
                .sessions(&project)
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
                "project": project,
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
        let root = authorize_project_path(&self.state.authorized_roots, &args.root, true)
            .map_err(|error| McpError::invalid_params(format!("repo_map root: {error}"), None))?;
        let mut files = 0usize;
        let mut total_bytes: u64 = 0;
        let mut signature_chars: usize = 0;
        let mut entries: Vec<serde_json::Value> = Vec::new();
        for entry in walk_files(&root) {
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
                .strip_prefix(&root)
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
        let target = authorize_project_path(&self.state.authorized_roots, &args.target, false)
            .map_err(|error| {
                McpError::invalid_params(format!("templates.scaffold target: {error}"), None)
            })?;
        let tmpl = rtrt_templates::find(&args.template).ok_or_else(|| {
            McpError::invalid_params(format!("unknown template: {}", args.template), None)
        })?;
        let plan = rtrt_templates::render::plan(&tmpl, &target, args.variables)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let id = store
            .set_block(&project, &args.name, &args.body)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let block = store
            .get_block(&project, &args.name)
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
        let project = self.state.pinned_project(args.project.as_deref())?;
        let store = self.state.memory()?.lock().await;
        let blocks = store
            .list_blocks(&project)
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
        let project = args.project.clone();
        self.state.pinned_project(project.as_deref())?;
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
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("provider gateway is disabled", None))?
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
            .auto_capture("provider_chat", project.as_deref(), &resp.content)
            .await?;
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
        let project = args.project.clone();
        let capture_project = self.state.pinned_project(project.as_deref())?;
        let provenance = args.invocation.resolve(Some(&capture_project));
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
                provenance,
            },
        )
        .await
        .map_err(|e| McpError::internal_error(format!("agent_call: {e}"), None))?;
        let body = serde_json::to_string(&outcome)
            .map_err(|e| McpError::internal_error(format!("agent_call serialize: {e}"), None))?;
        self.state
            .auto_capture("agent_call", Some(&capture_project), &outcome.output)
            .await?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Select the best detected agent route by preference and capability, optionally invoking the selected target. `prefer` is a ranking strategy (cheapest / local / quality), not a provider name: pin a provider with `target`, pin its upstream pool with `model`, and set `failover` to keep the ranked fallbacks behind the pick."
    )]
    async fn agent_route(
        &self,
        Parameters(args): Parameters<AgentRouteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let project = args.project.clone();
        let capture_project = self.state.pinned_project(project.as_deref())?;
        let provenance = args.invocation.resolve(Some(&capture_project));
        let req = agent_route_request(&args)?;
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
        let timeout = std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS);
        // `failover` walks the ranked candidates (the pick first, then its
        // sibling pools, then the rest); without it the single-shot invocation
        // is byte-for-byte the one this tool always made.
        let outcome = if req.failover {
            let ranked = decision.ranked_targets();
            let walked =
                invoke_with_failover_context(&ranked, &args.prompt, timeout, provenance.clone())
                    .await
                    .map_err(|e| McpError::internal_error(format!("agent_route: {e}"), None))?;
            body["served_by"] = serde_json::json!(walked.outcome.target);
            body["failover"] = serde_json::json!(walked.summary());
            body["failed_over"] = serde_json::to_value(&walked.failed_over).map_err(|e| {
                McpError::internal_error(format!("agent_route serialize: {e}"), None)
            })?;
            walked.outcome
        } else {
            invoke_agent(
                &decision.target,
                &args.prompt,
                InvokeOptions {
                    mode: Some(decision.mode),
                    model: decision.model,
                    timeout,
                    provenance,
                },
            )
            .await
            .map_err(|e| McpError::internal_error(format!("agent_route: {e}"), None))?
        };
        body["output"] = serde_json::Value::String(rtrt_compress::redact_secrets(&outcome.output));
        body["exit_code"] = serde_json::json!(outcome.exit_code);
        body["ms"] = serde_json::json!(outcome.ms);
        self.state
            .auto_capture("agent_route", Some(&capture_project), &outcome.output)
            .await?;
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
        let path = authorize_project_path(&self.state.authorized_roots, Path::new(&path), true)
            .map_err(|error| {
                McpError::invalid_params(format!("security.scan path: {error}"), None)
            })?;
        let report = rtrt_security::run(&profile, &path)
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

fn validate_http_auth(transport: Transport, token: Option<&str>) -> Result<()> {
    if matches!(transport, Transport::Http)
        && token.is_none_or(|value| {
            value.is_empty() || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
        })
    {
        anyhow::bail!("--transport http requires a non-empty RTRT_MCP_HTTP_TOKEN");
    }
    Ok(())
}

fn http_token_from_env(transport: Transport) -> Result<Option<String>> {
    let token = match transport {
        Transport::Stdio => None,
        Transport::Http => std::env::var("RTRT_MCP_HTTP_TOKEN").ok(),
    };
    validate_http_auth(transport, token.as_deref())?;
    Ok(token)
}

/// Decide whether a request's `Origin` header may proceed. An absent header is
/// a native (non-browser) client and is always allowed; an empty allowlist
/// admits no browser origin at all, so the default configuration cannot be
/// reached from a page.
fn origin_is_allowed(allowlist: &[String], origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    allowlist.iter().any(|allowed| allowed == origin)
}

async fn origin_guard(
    allowlist: Arc<Vec<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header::ORIGIN;
    let origin = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok());
    if origin_is_allowed(&allowlist, origin) {
        return next.run(req).await;
    }
    forbidden_origin_response()
}

fn forbidden_origin_response() -> axum::response::Response {
    let mut resp = axum::response::Response::new(axum::body::Body::from(
        "forbidden: origin not allowed for this rtrt-mcp endpoint",
    ));
    *resp.status_mut() = axum::http::StatusCode::FORBIDDEN;
    resp
}

/// Bearer-token guard for HTTP. Missing server configuration also denies every
/// request, providing defense in depth if startup validation is bypassed.
async fn bearer_guard(
    expected: Option<Arc<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header::AUTHORIZATION;
    let Some(expected) = expected else {
        return unauthorized_response();
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
    unauthorized_response()
}

fn unauthorized_response() -> axum::response::Response {
    use axum::http::{HeaderValue, StatusCode};
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
        if matches!(self.state.profile, RuntimeProfile::PermissionOnly) {
            return ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::from_build_env())
                .with_protocol_version(ProtocolVersion::V_2024_11_05)
                .with_instructions(
                    "RTRT permission broker bridge. Only permission_prompt is available; all other capabilities are disabled.",
                );
        }
        // Honest, runtime-accurate framing: hybrid recall is only live when an
        // embedder actually got attached at startup (embeddings enabled AND
        // Ollama reachable) — otherwise every memory tool is pure BM25.
        let recall_mode = if self.state.embedder.is_some() {
            "hybrid (BM25 + vector RRF) when project embedding coverage is meaningful, else BM25"
        } else {
            "BM25 (FTS5) — no embedder attached"
        };
        let llm_tools = match self.state.profile {
            RuntimeProfile::Http {
                process_execution: false,
                network: false,
            } => "LLM/process tools: disabled by HTTP capability profile.",
            RuntimeProfile::Http {
                process_execution: true,
                network: false,
            } => {
                "Provider invocation tools: disabled until both process-execution and network opt-ins are present."
            }
            RuntimeProfile::Http {
                process_execution: false,
                network: true,
            } => {
                "Network tools: provider_chat. Agent/provider execution tools require the process-execution opt-in too."
            }
            RuntimeProfile::Http {
                process_execution: true,
                network: true,
            }
            | RuntimeProfile::StdioFull
            | RuntimeProfile::AdminStdio => "LLM tools: provider_chat / agent_call / agent_route.",
            RuntimeProfile::PermissionOnly => unreachable!("handled above"),
        };
        let instructions = format!(
            "RTRT MCP server. Memory tools: memory_save (also opportunistically embeds new backlog when an embedder is attached) / \
                 memory_recall ({recall_mode}; payload filter always uses BM25) / \
                  memory_timeline (paginated history) / memory_profile (pinned-project stats) / \
                 memory_smart_search ({recall_mode}) / \
                 memory_relations (graph BFS from seed ids) / memory_export (JSONL) / \
                 memory_consolidate (archive oldest, keep most recent N) / \
                 memory_sessions (group rows by session_id, or list rows in one session) / \
                 memory_set_block / memory_get_block / memory_list_blocks (persona / human / context slots). \
                 Token tools: compress (rule rewriter) / compress_ml (token-importance) / \
                 proxy (command output filters). \
                 Code tools: repo_map (tree-sitter signatures). \
                 Project tools: templates_list / templates_scaffold. \
                 {llm_tools} \
                 Security tools: security_scan (profile-driven secrets / license / dependency / pattern / AI-artifact scan; profiles map to CWE/OWASP/NIST/CIS/SLSA/EU-AI-Act). \
                 Prompts: every entry in the local PromptRegistry (~/.rtrt/prompts) is exposed via prompts/list + prompts/get with handlebars argument substitution. \
                  Resources: only the startup-pinned memory://<project>/timeline and memory blocks are exposed."
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
        if !self.state.profile.allows_prompts_resources() {
            return Ok(rmcp::model::ListPromptsResult::default());
        }
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
        if !self.state.profile.allows_prompts_resources() {
            return Err(McpError::invalid_params("prompts are disabled", None));
        }
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
        if !self.state.profile.allows_prompts_resources() {
            return Ok(rmcp::model::ListResourcesResult::default());
        }
        let project = self.state.pinned_project(None)?;
        let store = self.state.memory()?.lock().await;
        let count = store
            .count_by_project(&project)
            .map_err(|e| McpError::internal_error(format!("resources/list: {e}"), None))?;
        let mut resources = Vec::new();
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
        let blocks = store
            .list_blocks(&project)
            .map_err(|e| McpError::internal_error(format!("resources/list blocks: {e}"), None))?;
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
        if !self.state.profile.allows_prompts_resources() {
            return Err(McpError::invalid_params("resources are disabled", None));
        }
        let uri = request.uri.clone();
        let parsed = parse_memory_uri(&uri).ok_or_else(|| {
            McpError::invalid_params(format!("unsupported resource URI: {uri}"), None)
        })?;
        let requested_project = match &parsed {
            MemoryUri::Timeline { project, .. } | MemoryUri::Block { project, .. } => project,
        };
        let project = self.state.pinned_project(Some(requested_project))?;
        let store = self.state.memory()?.lock().await;
        let body = match parsed {
            MemoryUri::Timeline { limit, .. } => {
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
            MemoryUri::Block { name, .. } => store
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
async fn build_embedder(cfg: &rtrt_core::Config, allow_network: bool) -> Option<Arc<dyn Embedder>> {
    if !allow_network {
        tracing::info!("startup embedder probing disabled by runtime capability profile");
        return None;
    }
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
    let profile = RuntimeProfile::from_cli(&cli)?;
    let http_token = http_token_from_env(cli.transport)?;
    if matches!(profile, RuntimeProfile::PermissionOnly) {
        let state = Arc::new(RtrtState {
            profile,
            memory: None,
            memory_binding: None,
            project: None,
            cfg: rtrt_core::Config::default(),
            embedder: None,
            gateway: None,
            prompts: None,
            auto_capture: false,
            auto_redact: true,
            session_id: String::new(),
            dedup_window_sec: 0,
            authorized_roots: Vec::new(),
        });
        tracing::info!("rtrt-mcp permission-only bridge starting on stdio");
        let service = RtrtMcp::with_state(state).serve(stdio()).await?;
        service.waiting().await?;
        return Ok(());
    }

    let current_dir = std::env::current_dir()?;
    let project = ProjectIdentity::derive(&current_dir)?;
    let authorized_root = canonical_authorized_root(project.checkout_root())?;
    let (memory, memory_binding, memory_path) = match &cli.memory {
        Some(path) => (
            MemoryStore::open(path)?,
            MemoryBinding::AdminPath(path.clone()),
            path.clone(),
        ),
        None => (
            MemoryStore::open_project(&project)?,
            MemoryBinding::Project(project.clone()),
            rtrt_core::project_memory_db_path(&project)?,
        ),
    };
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    let embedder = build_embedder(&cfg, profile.allows_startup_network()).await;
    let gateway = Some(Arc::new(Gateway::from_env()));
    let prompts = profile
        .allows_prompts_resources()
        .then(open_prompt_registry)
        .flatten();
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
    let session_id = Uuid::new_v4().to_string();
    let shared_state = Arc::new(RtrtState {
        profile,
        memory: Some(Mutex::new(memory)),
        memory_binding: Some(memory_binding),
        project: Some(project),
        cfg,
        embedder,
        gateway,
        prompts,
        auto_capture,
        auto_redact,
        session_id,
        dedup_window_sec,
        authorized_roots: vec![authorized_root],
    });
    match cli.transport {
        Transport::Stdio => {
            tracing::info!(
                "rtrt-mcp starting on stdio; memory={}",
                memory_path.display()
            );
            let service = RtrtMcp::with_state(shared_state).serve(stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http => {
            tracing::info!(
                "rtrt-mcp starting on http://{}{}; memory={}; auth={}; origins={}",
                cli.bind,
                cli.path,
                memory_path.display(),
                "bearer",
                if cli.allowed_origins.is_empty() {
                    "none (browser origins rejected)".into()
                } else {
                    cli.allowed_origins.join(",")
                },
            );
            let factory_state = shared_state.clone();
            let mut config = StreamableHttpServerConfig::default();
            config.allowed_origins = cli.allowed_origins.clone();
            let mcp_service = StreamableHttpService::new(
                move || Ok(RtrtMcp::with_state(factory_state.clone())),
                Arc::new(LocalSessionManager::default()),
                config,
            );
            let token = http_token.map(Arc::new);
            let origin_allowlist = Arc::new(cli.allowed_origins.clone());
            let app = axum::Router::new()
                .route_service(&cli.path, mcp_service)
                .layer(axum::middleware::from_fn(move |req, next| {
                    let token = token.clone();
                    async move { bearer_guard(token, req, next).await }
                }))
                .layer(axum::middleware::from_fn(move |req, next| {
                    let allowlist = origin_allowlist.clone();
                    async move { origin_guard(allowlist, req, next).await }
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
    fn empty_origin_allowlist_admits_native_clients_and_no_browser_origin() {
        assert!(origin_is_allowed(&[], None));
        assert!(!origin_is_allowed(&[], Some("http://localhost:7311")));
        assert!(!origin_is_allowed(&[], Some("https://evil.example")));
        assert!(!origin_is_allowed(&[], Some("null")));
    }

    #[test]
    fn configured_origin_allowlist_admits_only_exact_matches() {
        let allowlist = vec!["http://localhost:7311".to_string()];
        assert!(origin_is_allowed(&allowlist, None));
        assert!(origin_is_allowed(&allowlist, Some("http://localhost:7311")));
        assert!(!origin_is_allowed(
            &allowlist,
            Some("http://localhost:7312")
        ));
        assert!(!origin_is_allowed(
            &allowlist,
            Some("http://localhost:7311.evil.example")
        ));
        assert!(!origin_is_allowed(
            &allowlist,
            Some("HTTP://LOCALHOST:7311")
        ));
    }

    static PERMISSION_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    const PERMISSION_ENV_NAMES: [&str; 7] = [
        "RTRT_PERMISSION_BROKER_URL",
        "RTRT_PERMISSION_BROKER_TOKEN",
        "RTRT_PERMISSION_BROKER_NONCE",
        "RTRT_INVOCATION_ID",
        "RTRT_PARENT_SESSION_ID",
        "RTRT_PARENT_CALL_ID",
        "RTRT_CHILD_SESSION_ID",
    ];

    fn set_permission_env(port: u16) {
        let values = [
            (
                "RTRT_PERMISSION_BROKER_URL",
                format!("http://127.0.0.1:{port}{PERMISSION_BROKER_PATH}"),
            ),
            ("RTRT_PERMISSION_BROKER_TOKEN", "secret-token".into()),
            ("RTRT_PERMISSION_BROKER_NONCE", "secret-nonce".into()),
            ("RTRT_INVOCATION_ID", "invocation-1".into()),
            ("RTRT_PARENT_SESSION_ID", "parent-session-1".into()),
            ("RTRT_PARENT_CALL_ID", "parent-call-1".into()),
            ("RTRT_CHILD_SESSION_ID", "child-session-1".into()),
        ];
        for (name, value) in values {
            // SAFETY: all permission-adapter tests serialize environment access
            // with PERMISSION_ENV_LOCK and restore it before releasing the lock.
            unsafe { std::env::set_var(name, value) };
        }
    }

    fn clear_permission_env() {
        for name in PERMISSION_ENV_NAMES {
            // SAFETY: see set_permission_env.
            unsafe { std::env::remove_var(name) };
        }
    }

    fn permission_args() -> PermissionPromptArgs {
        PermissionPromptArgs {
            tool_name: "Bash".into(),
            input: serde_json::json!({"command":"cargo test"}),
            tool_use_id: Some("tool-use-1".into()),
        }
    }

    async fn test_request_permission(
        args: &PermissionPromptArgs,
    ) -> Result<bool, PermissionFailure> {
        tokio::time::timeout(std::time::Duration::from_secs(2), request_permission(args))
            .await
            .expect("permission request test timed out")
    }

    async fn permission_server(decision: &'static str) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind broker");
        let port = listener.local_addr().expect("broker address").port();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept broker request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.expect("read request");
                assert!(count > 0, "request ended before body");
                request.extend_from_slice(&chunk[..count]);
                let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..split]).expect("request headers");
                let length = headers
                    .split("\r\n")
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("content length");
                if request.len() >= split + 4 + length {
                    let body: serde_json::Value =
                        serde_json::from_slice(&request[split + 4..split + 4 + length])
                            .expect("request JSON");
                    let response = serde_json::json!({
                        "version": 1,
                        "request_id": body["request_id"],
                        "decision": decision,
                    })
                    .to_string();
                    let wire = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    );
                    socket
                        .write_all(wire.as_bytes())
                        .await
                        .expect("write response");
                    return request;
                }
            }
        });
        (port, task)
    }

    #[tokio::test]
    async fn permission_prompt_sends_exact_authenticated_request_and_maps_once() {
        let _guard = PERMISSION_ENV_LOCK.lock().await;
        clear_permission_env();
        let (port, server) = permission_server("once").await;
        set_permission_env(port);
        let result = test_request_permission(&permission_args()).await;
        clear_permission_env();
        assert!(matches!(result, Ok(true)));

        let request = server.await.expect("broker task");
        let split = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let headers = std::str::from_utf8(&request[..split]).unwrap();
        assert!(headers.starts_with(&format!("POST {PERMISSION_BROKER_PATH} HTTP/1.1\r\n")));
        assert!(headers.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")));
        assert!(headers.contains("\r\nAuthorization: Bearer secret-token\r\n"));
        assert!(headers.contains("\r\nX-RTRT-Broker-Nonce: secret-nonce\r\n"));
        assert!(headers.contains("\r\nContent-Type: application/json\r\n"));
        assert!(headers.ends_with("\r\nConnection: close"));
        let body: serde_json::Value = serde_json::from_slice(&request[split + 4..]).unwrap();
        assert_eq!(body["version"], 1);
        assert!(Uuid::parse_str(body["request_id"].as_str().unwrap()).is_ok());
        assert_eq!(body["broker_nonce"], "secret-nonce");
        assert_eq!(body["invocation_id"], "invocation-1");
        assert_eq!(body["parent_session_id"], "parent-session-1");
        assert_eq!(body["parent_call_id"], "parent-call-1");
        assert_eq!(body["child_session_id"], "child-session-1");
        assert_eq!(body["tool_name"], "Bash");
        assert_eq!(body["input"], serde_json::json!({"command":"cargo test"}));
        assert_eq!(body["tool_use_id"], "tool-use-1");
    }

    #[tokio::test]
    async fn permission_prompt_maps_always_and_reject() {
        let _guard = PERMISSION_ENV_LOCK.lock().await;
        for (decision, allowed) in [("always", true), ("reject", false)] {
            clear_permission_env();
            let (port, server) = permission_server(decision).await;
            set_permission_env(port);
            let result = test_request_permission(&permission_args()).await;
            assert_eq!(result.ok(), Some(allowed));
            server.await.expect("broker task");
        }
        clear_permission_env();
    }

    #[tokio::test]
    async fn permission_prompt_defaults_to_deny_for_missing_env_and_invalid_url() {
        let _guard = PERMISSION_ENV_LOCK.lock().await;
        clear_permission_env();
        assert!(matches!(
            test_request_permission(&permission_args()).await,
            Err(PermissionFailure::Configuration)
        ));
        for invalid in [
            "http://localhost:1234/rtrt/permission/v1",
            "http://[::1]:1234/rtrt/permission/v1",
            "http://user@127.0.0.1:1234/rtrt/permission/v1",
            "http://127.0.0.1:0/rtrt/permission/v1",
            "http://127.0.0.1:1234/rtrt/permission/v1?x=1",
            "http://127.0.0.1:1234/rtrt/permission/v1#x",
            "http://127.0.0.1:1234/other",
        ] {
            set_permission_env(1);
            // SAFETY: serialized by PERMISSION_ENV_LOCK.
            unsafe { std::env::set_var("RTRT_PERMISSION_BROKER_URL", invalid) };
            assert!(
                matches!(
                    test_request_permission(&permission_args()).await,
                    Err(PermissionFailure::Configuration)
                ),
                "accepted {invalid}"
            );
        }
        clear_permission_env();
    }

    #[tokio::test]
    async fn permission_prompt_rejects_malformed_mismatched_and_oversized_responses() {
        async fn run(response: Vec<u8>) -> PermissionFailure {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            set_permission_env(port);
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0_u8; 4096];
                let _ = socket.read(&mut buffer).await;
                socket.write_all(&response).await.unwrap();
            });
            let error = test_request_permission(&permission_args())
                .await
                .expect_err("response must be denied");
            task.await.unwrap();
            error
        }

        let _guard = PERMISSION_ENV_LOCK.lock().await;
        clear_permission_env();
        let malformed = b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n{".to_vec();
        assert!(matches!(run(malformed).await, PermissionFailure::Response));
        let mismatch_body = r#"{"version":1,"request_id":"different","decision":"once"}"#;
        let mismatch = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{mismatch_body}",
            mismatch_body.len()
        )
        .into_bytes();
        assert!(matches!(run(mismatch).await, PermissionFailure::Response));
        assert!(matches!(
            run(vec![b'x'; PERMISSION_MAX_RESPONSE + 1]).await,
            PermissionFailure::Response
        ));
        clear_permission_env();
    }

    #[tokio::test]
    async fn permission_prompt_connect_failure_and_request_bounds_deny() {
        let _guard = PERMISSION_ENV_LOCK.lock().await;
        clear_permission_env();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        set_permission_env(listener.local_addr().unwrap().port());
        drop(listener);
        let connect_failure = test_request_permission(&permission_args()).await;
        assert!(matches!(
            connect_failure,
            Err(PermissionFailure::Unavailable)
        ));

        let invalid = PermissionPromptArgs {
            tool_name: "Bash".into(),
            input: serde_json::Value::String("not an object".into()),
            tool_use_id: None,
        };
        assert!(matches!(
            test_request_permission(&invalid).await,
            Err(PermissionFailure::Request)
        ));

        let whitespace_name = PermissionPromptArgs {
            tool_name: " \t ".into(),
            input: serde_json::json!({}),
            tool_use_id: None,
        };
        assert!(matches!(
            test_request_permission(&whitespace_name).await,
            Err(PermissionFailure::Request)
        ));

        // SAFETY: serialized by PERMISSION_ENV_LOCK.
        unsafe { std::env::set_var("RTRT_INVOCATION_ID", "bad\nidentity") };
        assert!(matches!(
            test_request_permission(&permission_args()).await,
            Err(PermissionFailure::Configuration)
        ));
        clear_permission_env();
    }

    #[test]
    fn permission_prompt_schema_and_registration_match_claude_contract() {
        let schema = rmcp::schemars::schema_for!(PermissionPromptArgs);
        let value = serde_json::to_value(schema).unwrap();
        let properties = value["properties"].as_object().expect("schema properties");
        assert!(properties.contains_key("tool_name"));
        let input = properties["input"]
            .as_object()
            .expect("permission input must use an object schema");
        assert_eq!(
            input.get("type").and_then(serde_json::Value::as_str),
            Some("object")
        );
        assert!(properties.contains_key("tool_use_id"));
        let tools = RtrtMcp::tool_router().list_all();
        assert!(
            tools
                .iter()
                .any(|tool| tool.name.as_ref() == "permission_prompt")
        );
    }

    #[test]
    fn invocation_context_requires_an_id_and_uses_only_server_project_attribution() {
        assert!(
            InvocationContextArgs::default()
                .resolve(Some("project"))
                .is_none()
        );

        let args = InvocationContextArgs {
            invocation_id: Some("invocation-1".into()),
            parent_project: Some("injected-project".into()),
            parent_session_id: Some("session-1".into()),
            parent_call_id: Some("call-1".into()),
            caller_agent: Some("build".into()),
            parent_cwd: Some("/tmp/injected-cwd".into()),
            parent_worktree: Some("/repo/injected-worktree".into()),
        };
        let context = args.resolve(Some("explicit")).expect("resolved context");

        assert_eq!(context.invocation_id, "invocation-1");
        assert_eq!(context.parent_project.as_deref(), Some("explicit"));
        assert_eq!(context.parent_session_id.as_deref(), Some("session-1"));
        assert_eq!(context.parent_call_id.as_deref(), Some("call-1"));
        assert_eq!(context.caller_agent.as_deref(), Some("build"));

        let cwd_context = InvocationContextArgs {
            parent_worktree: None,
            ..args.clone()
        }
        .resolve(Some("generic-project"))
        .expect("cwd context");
        assert_eq!(
            cwd_context.parent_project.as_deref(),
            Some("generic-project")
        );

        let named_context = InvocationContextArgs {
            parent_worktree: None,
            parent_cwd: None,
            ..args
        }
        .resolve(Some("generic-project"))
        .expect("named context");
        assert_eq!(
            named_context.parent_project.as_deref(),
            Some("generic-project")
        );
    }

    #[test]
    fn ct_eq_basic_cases() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn http_transport_requires_valid_bearer_configuration_but_stdio_does_not() {
        assert!(validate_http_auth(Transport::Http, None).is_err());
        assert!(validate_http_auth(Transport::Http, Some("")).is_err());
        assert!(validate_http_auth(Transport::Http, Some("bad\ntoken")).is_err());
        assert!(validate_http_auth(Transport::Http, Some("secret")).is_ok());
        assert!(validate_http_auth(Transport::Stdio, None).is_ok());
    }

    #[test]
    fn cli_rejects_http_token_argument() {
        // Given a direct HTTP invocation containing the removed secret flag.
        let args = ["rtrt-mcp", "--transport", "http", "--http-token", "secret"];

        // When clap parses the invocation.
        let result = Cli::try_parse_from(args);

        // Then the secret-bearing argument is rejected.
        assert!(result.is_err());
    }

    #[test]
    fn http_auth_reads_token_from_environment() {
        // Given a valid token only in the HTTP authentication environment.
        let _guard = PERMISSION_ENV_LOCK.blocking_lock();
        let previous = std::env::var_os("RTRT_MCP_HTTP_TOKEN");
        // SAFETY: this test serializes access to RTRT_MCP_HTTP_TOKEN and restores it below.
        unsafe { std::env::set_var("RTRT_MCP_HTTP_TOKEN", "environment-secret") };

        // When HTTP authentication configuration loads without a token argument.
        let token = http_token_from_env(Transport::Http);

        // Then the environment token is accepted without a CLI argument.
        // SAFETY: see the serialized environment access above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("RTRT_MCP_HTTP_TOKEN", value),
                None => std::env::remove_var("RTRT_MCP_HTTP_TOKEN"),
            }
        }
        assert_eq!(token.unwrap().as_deref(), Some("environment-secret"));
    }

    fn cli_for_profile(transport: Transport) -> Cli {
        Cli {
            memory: None,
            admin: false,
            transport,
            bind: "127.0.0.1:7312".into(),
            path: "/mcp".into(),
            allowed_origins: Vec::new(),
            permission_only: false,
            http_allow_process_execution: false,
            http_allow_network: false,
        }
    }

    #[test]
    fn runtime_profile_rejects_invalid_flag_combinations() {
        let mut cli = cli_for_profile(Transport::Http);
        cli.permission_only = true;
        assert!(RuntimeProfile::from_cli(&cli).is_err());

        let mut cli = cli_for_profile(Transport::Stdio);
        cli.http_allow_process_execution = true;
        assert!(RuntimeProfile::from_cli(&cli).is_err());
        cli.http_allow_process_execution = false;
        cli.http_allow_network = true;
        assert!(RuntimeProfile::from_cli(&cli).is_err());

        let mut cli = cli_for_profile(Transport::Stdio);
        cli.memory = Some(PathBuf::from("memory.sqlite"));
        assert!(RuntimeProfile::from_cli(&cli).is_err());
        cli.admin = true;
        assert_eq!(
            RuntimeProfile::from_cli(&cli).unwrap(),
            RuntimeProfile::AdminStdio
        );

        let mut cli = cli_for_profile(Transport::Http);
        cli.memory = Some(PathBuf::from("memory.sqlite"));
        cli.admin = true;
        assert!(RuntimeProfile::from_cli(&cli).is_err());
    }

    fn server_for_profile(profile: RuntimeProfile, tag: &str) -> (RtrtMcp, PathBuf) {
        let path = temp_store_path(tag);
        let mut state = test_state(path.clone(), rtrt_core::Config::default(), None);
        state.profile = profile;
        (RtrtMcp::with_state(Arc::new(state)), path)
    }

    fn listed_tool_names(server: &RtrtMcp) -> Vec<String> {
        server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
    }

    #[test]
    fn permission_only_lists_only_permission_prompt_and_disables_other_capabilities() {
        let (server, path) = server_for_profile(RuntimeProfile::PermissionOnly, "permission-only");
        assert_eq!(listed_tool_names(&server), ["permission_prompt"]);
        assert!(server.tool_router.is_disabled("compress"));
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
        // A real permission-only startup uses the same shape without opening
        // memory, providers, prompts, or filesystem roots.
        assert!(server.tool_router.get("memory_save").is_none());
        cleanup_store(&path);
    }

    #[test]
    fn permission_only_state_needs_no_db_network_prompts_or_project() {
        let state = RtrtState {
            profile: RuntimeProfile::PermissionOnly,
            memory: None,
            memory_binding: None,
            project: None,
            cfg: rtrt_core::Config::default(),
            embedder: None,
            gateway: None,
            prompts: None,
            auto_capture: false,
            auto_redact: true,
            session_id: String::new(),
            dedup_window_sec: 0,
            authorized_roots: Vec::new(),
        };
        let server = RtrtMcp::with_state(Arc::new(state));
        assert_eq!(listed_tool_names(&server), ["permission_prompt"]);
        assert!(server.state.memory.is_none());
        assert!(server.state.gateway.is_none());
        assert!(server.state.prompts.is_none());
        assert!(server.state.authorized_roots.is_empty());
    }

    #[test]
    fn http_tool_listing_follows_process_and_network_opt_ins() {
        let profiles = [
            (
                RuntimeProfile::Http {
                    process_execution: false,
                    network: false,
                },
                false,
                false,
            ),
            (
                RuntimeProfile::Http {
                    process_execution: true,
                    network: false,
                },
                true,
                false,
            ),
            (
                RuntimeProfile::Http {
                    process_execution: false,
                    network: true,
                },
                false,
                true,
            ),
            (
                RuntimeProfile::Http {
                    process_execution: true,
                    network: true,
                },
                true,
                true,
            ),
        ];
        for (index, (profile, process, network)) in profiles.into_iter().enumerate() {
            let (server, path) = server_for_profile(profile, &format!("http-profile-{index}"));
            let names = listed_tool_names(&server);
            for name in ["agent_call", "agent_route"] {
                assert_eq!(
                    names.iter().any(|listed| listed == name),
                    process && network,
                    "{name}"
                );
            }
            assert_eq!(
                names.iter().any(|listed| listed == "provider_chat"),
                network
            );
            let instructions = server.get_info().instructions.unwrap_or_default();
            for name in ["agent_call", "agent_route"] {
                assert_eq!(
                    instructions.contains(name),
                    process && network,
                    "instructions: {name}"
                );
            }
            assert_eq!(instructions.contains("provider_chat"), network);
            assert!(names.iter().any(|name| name == "compress"));
            cleanup_store(&path);
        }
    }

    #[test]
    fn http_capability_classification_requires_every_opt_in_and_fails_closed() {
        let execution = tool_capabilities("agent_call").unwrap();
        assert!(!execution.allowed(false, false));
        assert!(!execution.allowed(true, false));
        assert!(!execution.allowed(false, true));
        assert!(execution.allowed(true, true));

        let network = tool_capabilities("provider_chat").unwrap();
        assert!(!network.allowed(true, false));
        assert!(network.allowed(false, true));
        assert!(tool_capabilities("future_execution_tool").is_none());
    }

    #[test]
    fn full_stdio_preserves_every_registered_tool_and_capability() {
        let (server, path) = server_for_profile(RuntimeProfile::StdioFull, "stdio-full");
        let expected = RtrtMcp::tool_router().list_all();
        assert_eq!(server.tool_router.list_all().len(), expected.len());
        for tool in expected {
            assert!(server.tool_router.has_route(tool.name.as_ref()));
        }
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_some());
        assert!(info.capabilities.resources.is_some());
        cleanup_store(&path);
    }

    #[test]
    fn disabled_tools_reject_direct_router_lookup() {
        fn denied(server: &RtrtMcp, name: &'static str) {
            assert!(server.tool_router.is_disabled(name));
            assert!(
                server.tool_router.get(name).is_none(),
                "disabled direct call route must be unavailable: {name}"
            );
        }

        let (permission, permission_path) =
            server_for_profile(RuntimeProfile::PermissionOnly, "permission-denial");
        denied(&permission, "compress");

        let (http_default, http_default_path) = server_for_profile(
            RuntimeProfile::Http {
                process_execution: false,
                network: false,
            },
            "http-default-denial",
        );
        denied(&http_default, "agent_call");
        denied(&http_default, "agent_route");
        denied(&http_default, "provider_chat");

        let (process_only, process_path) = server_for_profile(
            RuntimeProfile::Http {
                process_execution: true,
                network: false,
            },
            "http-network-denial",
        );
        denied(&process_only, "provider_chat");
        denied(&process_only, "agent_call");
        denied(&process_only, "agent_route");

        let (network_only, network_path) = server_for_profile(
            RuntimeProfile::Http {
                process_execution: false,
                network: true,
            },
            "http-process-denial",
        );
        denied(&network_only, "agent_call");

        for path in [
            permission_path,
            http_default_path,
            process_path,
            network_path,
        ] {
            cleanup_store(&path);
        }
    }

    #[tokio::test]
    async fn http_default_skips_startup_embedder_network_probe() {
        let profile = RuntimeProfile::Http {
            process_execution: false,
            network: false,
        };
        assert!(!profile.allows_startup_network());
        let mut cfg = rtrt_core::Config::default();
        cfg.embeddings.enabled = true;
        cfg.embeddings.base_url = Some("http://127.0.0.1:1".into());
        assert!(
            build_embedder(&cfg, profile.allows_startup_network())
                .await
                .is_none()
        );
    }

    fn temp_project_root(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rtrt-mcp-path-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn project_path_gate_rejects_root_external_prefix_dotdot_and_allows_local_targets() {
        let root = temp_project_root("gate");
        let sibling = root.with_file_name(format!(
            "{}-sibling",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir(&sibling).unwrap();
        let roots = vec![root.clone()];

        assert!(authorize_project_path(&roots, Path::new("."), true).is_ok());
        assert_eq!(
            authorize_project_path(&roots, Path::new("new/nested"), false).unwrap(),
            root.join("new/nested")
        );
        assert!(authorize_project_path(&roots, Path::new("../escape"), false).is_err());
        assert!(authorize_project_path(&roots, &sibling, true).is_err());
        assert!(authorize_project_path(&roots, Path::new("/"), true).is_err());

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(sibling).unwrap();
    }

    #[test]
    fn project_path_gate_enforces_single_authorized_root() {
        let first = temp_project_root("single-root-first");
        let second = temp_project_root("single-root-second");
        assert!(authorize_project_path(&[], Path::new("."), true).is_err());
        let error = authorize_project_path(&[first.clone(), second.clone()], Path::new("."), true)
            .unwrap_err();
        assert!(error.contains("exactly one authorized project root is supported"));
        std::fs::remove_dir_all(first).unwrap();
        std::fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn same_basename_roots_get_isolated_project_stores_and_worktrees_share_identity() {
        let root = temp_project_root("identity-integration");
        let home = root.join("home");
        let first = root.join("one/repo");
        let second = root.join("two/repo");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir_all(first.join(".git/worktrees/linked")).unwrap();
        std::fs::create_dir_all(second.join(".git")).unwrap();
        let linked = root.join("linked");
        std::fs::create_dir(&linked).unwrap();
        let gitdir = first.join(".git/worktrees/linked");
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            gitdir.join("gitdir"),
            format!("{}\n", linked.join(".git").display()),
        )
        .unwrap();

        let first_id = ProjectIdentity::derive(&first).unwrap();
        let second_id = ProjectIdentity::derive(&second).unwrap();
        let linked_id = ProjectIdentity::derive(&linked).unwrap();
        assert_eq!(first_id.label(), second_id.label());
        assert_ne!(first_id.slug(), second_id.slug());
        assert_eq!(first_id.slug(), linked_id.slug());
        assert_ne!(first_id.checkout_root(), linked_id.checkout_root());

        let first_store = MemoryStore::open_project_in(&first_id, &home).unwrap();
        let second_store = MemoryStore::open_project_in(&second_id, &home).unwrap();
        first_store.save(first_id.slug(), "note", "first").unwrap();
        second_store
            .save(second_id.slug(), "note", "second")
            .unwrap();
        assert_eq!(first_store.count_by_project(first_id.slug()).unwrap(), 1);
        assert_eq!(second_store.count_by_project(second_id.slug()).unwrap(), 1);
        drop(first_store);
        drop(second_store);
        let linked_store = MemoryStore::open_project_in(&linked_id, &home).unwrap();
        assert_eq!(linked_store.count_by_project(linked_id.slug()).unwrap(), 1);
        drop(linked_store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_path_gate_rejects_symlink_roots_children_and_nonexistent_escapes() {
        use std::os::unix::fs::symlink;

        let root = temp_project_root("symlink-root");
        let outside = temp_project_root("symlink-outside");
        symlink(&outside, root.join("linked")).unwrap();
        symlink(&root, outside.join("root-link")).unwrap();
        let roots = vec![root.clone()];

        assert!(authorize_project_path(&roots, &outside.join("root-link"), true).is_err());
        assert!(authorize_project_path(&roots, Path::new("linked"), true).is_err());
        assert!(
            authorize_project_path(&roots, Path::new("linked/not-created/yet"), false).is_err()
        );

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
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
    fn agent_route_capability_error_lists_the_accepted_values() {
        let err = parse_agent_route_capability(Some("implementation")).unwrap_err();
        let message = err.message.to_string();
        assert!(
            message.contains("expected code, reasoning, vision, embed, agentic, or cheap"),
            "{message}"
        );
    }

    /// The plain rejection: no detected identity matches, so the message is the
    /// accepted-value list and nothing invented.
    #[test]
    fn agent_route_prefer_error_lists_the_accepted_values() {
        let err = parse_agent_route_prefer_with(Some("telepathy"), |_| None).unwrap_err();
        assert_eq!(
            err.message.to_string(),
            "agent_route prefer: unknown prefer 'telepathy' (expected cheapest, local, or quality)"
        );
    }

    /// The mistake that started this: a provider name passed to `prefer`.
    #[test]
    fn agent_route_prefer_error_points_a_target_name_at_target() {
        let err = parse_agent_route_prefer_with(Some("opencode"), |value| {
            detected_route_in(&[pooled_tool()], value)
        })
        .unwrap_err();
        assert_eq!(
            err.message.to_string(),
            "agent_route prefer: unknown prefer 'opencode' (expected cheapest, local, or quality). \
             'opencode' is a detected target, not a strategy: pass target=\"opencode\" instead"
        );
    }

    /// A pool name is not a target either — the fix needs `model` as well.
    #[test]
    fn agent_route_prefer_error_points_a_pool_name_at_target_and_model() {
        let err = parse_agent_route_prefer_with(Some("opencode-go"), |value| {
            detected_route_in(&[pooled_tool()], value)
        })
        .unwrap_err();
        assert_eq!(
            err.message.to_string(),
            "agent_route prefer: unknown prefer 'opencode-go' (expected cheapest, local, or \
             quality). 'opencode-go' is a pool of the detected target 'opencode', not a strategy: \
             pass target=\"opencode\" (with model=\"opencode-go/glm-5.2\" to pin that pool) instead"
        );
    }

    #[test]
    fn detected_route_resolves_targets_pools_and_nothing_else() {
        let tools = [pooled_tool()];
        assert_eq!(
            detected_route_in(&tools, " OpenCode "),
            Some(DetectedRoute::Target {
                target: "opencode".to_string()
            })
        );
        assert_eq!(
            detected_route_in(&tools, "ollama"),
            Some(DetectedRoute::Pool {
                target: "opencode".to_string(),
                pool: "ollama".to_string(),
                model: "ollama/glm-5.2:cloud".to_string(),
            })
        );
        assert_eq!(detected_route_in(&tools, "telepathy"), None);
        assert_eq!(detected_route_in(&tools, "  "), None);
    }

    /// Backward compatibility: the pre-existing argument shape still builds the
    /// request it always built — no target, no mode, no failover.
    #[test]
    fn agent_route_legacy_arguments_build_the_legacy_request() {
        let args: AgentRouteArgs =
            serde_json::from_str(r#"{"prompt":"ship it","capability":"code"}"#)
                .expect("valid arguments");
        assert_eq!(args.dry_run, None);
        let req = agent_route_request(&args).expect("legacy arguments route");
        assert_eq!(
            req,
            RouteRequest {
                capability: Some(Capability::Code),
                prefer: Prefer::Cheapest,
                target: None,
                model: None,
                mode: None,
                failover: false,
            }
        );
    }

    #[test]
    fn agent_route_arguments_carry_target_mode_and_failover() {
        let args: AgentRouteArgs = serde_json::from_str(
            r#"{"prompt":"ship it","target":"opencode","mode":"CLI","failover":true}"#,
        )
        .expect("valid arguments");
        let req = agent_route_request(&args).expect("pinned arguments route");
        assert_eq!(req.target.as_deref(), Some("opencode"));
        assert_eq!(req.mode, Some(InvokeMode::Cli));
        assert!(req.failover);
        assert!(
            agent_route_request(&AgentRouteArgs {
                mode: Some("telekinesis".to_string()),
                ..AgentRouteArgs::default()
            })
            .is_err()
        );
    }

    /// An explicit `target` pins that target; `failover` keeps the ranked
    /// fallbacks behind it instead of collapsing the route to one candidate.
    #[test]
    fn agent_route_target_pins_and_failover_keeps_alternatives() {
        let tools = routing_tools();
        let usage = UsageSnapshot::default();

        let pinned = agent_route_request(&AgentRouteArgs {
            target: Some("opencode".to_string()),
            ..AgentRouteArgs::default()
        })
        .expect("pinned request");
        let decision = select_route(&pinned, &tools, &usage).expect("pinned route");
        assert_eq!(decision.target, "opencode");
        assert!(decision.alternatives.is_empty(), "{decision:?}");

        let with_failover = agent_route_request(&AgentRouteArgs {
            target: Some("opencode".to_string()),
            failover: Some(true),
            ..AgentRouteArgs::default()
        })
        .expect("failover request");
        let decision = select_route(&with_failover, &tools, &usage).expect("failover route");
        assert_eq!(decision.target, "opencode");
        assert!(!decision.alternatives.is_empty(), "{decision:?}");
        assert!(decision.ranked_targets().len() >= 2, "{decision:?}");
    }

    /// `opencode` reaching two upstream pools, plus a second target to fall over
    /// to — the shape the owner actually runs.
    fn pooled_tool() -> DetectedTool {
        DetectedTool {
            name: "opencode".to_string(),
            kind: rtrt_core::ToolKind::CodingAgent,
            installed: true,
            path: None,
            version: None,
            invocation_modes: vec![rtrt_core::InvocationMode::Cli],
            cli_invocation: Some("opencode run {prompt}".to_string()),
            cost_class: rtrt_core::CostClass::SubscriptionFlat,
            capabilities: vec![Capability::Code],
            config_path: None,
            models: vec![
                "opencode-go/glm-5.2".to_string(),
                "ollama/glm-5.2:cloud".to_string(),
            ],
            server_running: None,
            enabled: true,
        }
    }

    fn routing_tools() -> Vec<DetectedTool> {
        let mut claude = pooled_tool();
        claude.name = "claude".to_string();
        claude.cli_invocation = Some("claude -p {prompt}".to_string());
        claude.models = vec!["sonnet".to_string()];
        vec![pooled_tool(), claude]
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
        let project = ProjectIdentity::derive(std::env::current_dir().unwrap()).unwrap();
        RtrtState {
            profile: RuntimeProfile::StdioFull,
            memory: Some(Mutex::new(memory)),
            memory_binding: Some(MemoryBinding::AdminPath(path)),
            project: Some(project),
            cfg,
            embedder,
            gateway: Some(Arc::new(Gateway::new())),
            prompts: None,
            auto_capture: false,
            auto_redact: true,
            session_id: "test-session".to_string(),
            dedup_window_sec: 0,
            authorized_roots: vec![std::env::current_dir().unwrap().canonicalize().unwrap()],
        }
    }

    #[tokio::test]
    async fn auto_capture_is_pinned_and_rejects_provenance_spoofing() {
        let path = temp_store_path("shared-project");
        let mut state = test_state(path.clone(), rtrt_core::Config::default(), None);
        state.auto_capture = true;
        let project = state.project.as_ref().unwrap().slug().to_string();
        state
            .auto_capture("agent_call", None, "pinned")
            .await
            .unwrap();
        let provenance = InvocationContext {
            parent_project: Some("provenance-project".into()),
            ..InvocationContext::default()
        };
        assert_eq!(
            provenance.parent_project.as_deref(),
            Some("provenance-project")
        );
        assert!(
            state
                .auto_capture(
                    "agent_call",
                    provenance.parent_project.as_deref(),
                    "spoofed"
                )
                .await
                .is_err()
        );

        let store = state.memory().unwrap().lock().await;
        assert_eq!(store.count_by_project(&project).unwrap(), 1);
        assert_eq!(store.count_by_project("provenance-project").unwrap(), 0);
        drop(store);
        cleanup_store(&path);
    }

    #[test]
    fn project_arguments_and_resource_uris_are_server_pinned() {
        let path = temp_store_path("project-assertions");
        let state = test_state(path.clone(), rtrt_core::Config::default(), None);
        let identity = state.project.as_ref().unwrap();
        for value in [
            None,
            Some(""),
            Some(identity.slug()),
            Some(identity.label()),
        ] {
            assert_eq!(state.pinned_project(value).unwrap(), identity.slug());
        }
        assert!(state.pinned_project(Some("foreign-project")).is_err());

        let local = format!("memory://{}/timeline", identity.slug());
        let foreign = "memory://foreign-project/timeline";
        let MemoryUri::Timeline { project, .. } = parse_memory_uri(&local).unwrap() else {
            panic!("timeline URI expected")
        };
        assert!(state.pinned_project(Some(&project)).is_ok());
        let MemoryUri::Timeline { project, .. } = parse_memory_uri(foreign).unwrap() else {
            panic!("timeline URI expected")
        };
        assert!(state.pinned_project(Some(&project)).is_err());
        cleanup_store(&path);
    }

    #[tokio::test]
    async fn id_based_relations_reject_rows_outside_pinned_project() {
        let path = temp_store_path("foreign-id");
        let state = Arc::new(test_state(path.clone(), rtrt_core::Config::default(), None));
        let foreign_id = state
            .memory()
            .unwrap()
            .lock()
            .await
            .save("foreign-project", "note", "foreign")
            .unwrap();
        let server = RtrtMcp::with_state(state);
        let error = server
            .memory_relations(Parameters(MemoryRelationsArgs {
                project: None,
                seed_ids: vec![foreign_id],
                depth: 1,
            }))
            .await
            .unwrap_err();
        assert!(error.message.contains("does not belong to pinned project"));
        cleanup_store(&path);
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
            let store = state.memory().unwrap().lock().await;
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
        cfg.embeddings.model = MockEmbedder.model_name().to_string();
        let state = test_state(path.clone(), cfg, Some(Arc::new(MockEmbedder)));

        {
            let store = state.memory().unwrap().lock().await;
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
            let store = state.memory().unwrap().lock().await;
            store.save("p", "note", "alpha").unwrap();
            store.save("p", "note", "beta").unwrap();
        }
        assert_eq!(
            state
                .memory()
                .unwrap()
                .lock()
                .await
                .unembedded_count()
                .unwrap(),
            2,
            "nothing embedded yet"
        );

        state.spawn_opportunistic_embed_sweep();
        // The sweep runs on a detached blocking task; give it a moment to
        // finish rather than asserting on an unpredictable race.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let remaining = state
            .memory()
            .unwrap()
            .lock()
            .await
            .unembedded_count()
            .unwrap();
        // Backlog of 2 -> batch size sqrt(2).ceil() == 2, so the sweep should
        // have cleared the whole backlog in one hop.
        assert_eq!(remaining, 0);
        cleanup_store(&path);
    }
}
