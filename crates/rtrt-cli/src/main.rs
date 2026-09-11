//! rtrt (Retort) — top-level CLI for the Rust toolkit that distills AI agent context.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
mod doctor;
pub mod opencode_sessions;
mod proxy_stats;
mod sandbox;
mod security;
mod service;
mod setup;

use rtrt_compress::{
    AsyncCompressor, Compressor, Language as TsLanguage, LlmCompressor, SignatureExtractor,
};
use rtrt_core::{
    Capability, CompressionLevel, CostClass, DetectedTool, InvocationMode, OutputStyleLevel,
    PoolKey, ProjectIdentity, ToolKind,
};
use rtrt_memory::{
    Embedder, InvocationProvenance, LlmSummariser, MemoryStore, OllamaEmbedder,
    is_synthetic_prompt, truncate_for_embed,
};
use rtrt_providers::{
    AnthropicProvider, ChatMessage, ChatRequest, ChatStreamEvent, Context7Client,
    DEFAULT_GATEWAY_HOST, DEFAULT_GATEWAY_PORT, DEFAULT_TIMEOUT_SECS, InvokeOptions,
    Mode as InvokeMode, OpenAICompatibleProvider, OpenAIProvider, PoolCap, PoolHeadroom, Prefer,
    Provider, RankedTarget, Role, RoomBasis, RouteDecision, RouteRequest, TargetHeadroom,
    TargetWindows, UsageSnapshot, gateway_default_timeout, headroom_for_pool, invoke_agent,
    invoke_with_failover, provider_usage_windows, rank_pools_by_room, record_invocation,
    select_route, serve_gateway, target_headroom,
};
use rtrt_templates::PromptRegistry;
use setup::{
    AgentKind, CheckState as ProjectCheckState, ClaudeSettingsStatus, SetupPlan,
    claude_settings_status, memory_reachable_status,
};

/// Grouped command overview shown at the bottom of `rtrt --help`. Every
/// top-level command is hidden from clap's default flat listing
/// (`#[command(hide = true)]`) so this hand-curated, screen-fitting summary
/// is the only command index — `rtrt <command> --help` still shows each
/// command's full about/long_about individually.
const CLI_AFTER_HELP: &str = "\
Command groups (run `rtrt <command> --help` for details):
  Savings & Analytics  compress, stats, gain, proxy, proxy-run, discover,
                       benchmark, repo-map, run, context
  Memory               memory
  Routing & Providers  provider, call, route, usage, diagnose
  Project              templates, new, init, migrate, project, opencode, docs,
                       security
  Setup & Install      setup, uninstall, mcp, service, detect, config, info,
                       doctor, prompt

Quickstart: rtrt setup --agent claude --apply --plugin  ->  rtrt doctor  ->  rtrt gain";

#[derive(Debug, Parser)]
#[command(
    name = "rtrt",
    version,
    about = "Retort — a Rust toolkit that distills AI agent context (memory, compression, proxy, routing)",
    long_about = None,
    override_usage = "rtrt <COMMAND> [ARGS]...",
    after_help = CLI_AFTER_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Launch OpenCode with project-private data and state.
    #[command(hide = true)]
    Opencode {
        /// Project checkout (defaults to cwd).
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
        #[command(subcommand)]
        action: Option<OpenCodeAction>,
        /// OpenCode arguments. The `--` separator is mandatory.
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Compress text read from stdin or --file.
    #[command(hide = true)]
    Compress {
        /// Compression level. When omitted, defaults to the repo's effective
        /// per-project `[compression] level` (`<repo>/.rtrt/config.toml`),
        /// else the global default (`full`).
        #[arg(short, long, value_enum)]
        level: Option<LevelArg>,
        /// Read input from a file instead of stdin.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
        /// Overwrite --file with the compressed output.
        #[arg(long)]
        in_place: bool,
        /// Before --in-place overwrite, copy --file to <PATH>.original.
        #[arg(long)]
        backup: bool,
        /// Use an LLM (any Provider) to rewrite tersely instead of the rule
        /// pass. Required when --provider is set.
        #[arg(long)]
        llm: bool,
        /// Provider (with --llm). Auto-detected from --model otherwise.
        #[arg(long, value_enum)]
        provider: Option<ProviderArg>,
        /// Model id (with --llm). e.g. `claude-haiku-4-5`, `gpt-5.4-mini`,
        /// `llama3.2` (for openai-compat against Ollama).
        #[arg(long)]
        model: Option<String>,
        /// Base URL for `--provider openai-compat` (e.g. `http://127.0.0.1:11434/v1`).
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        /// Output framing — chroma-style multi-format.
        #[arg(long, value_enum, default_value = "plain")]
        format: FormatArg,
        /// Use the LLMLingua-style ML compressor (token importance scoring)
        /// instead of the rule engine. Mutually exclusive with --llm.
        #[arg(long, conflicts_with = "llm")]
        ml: bool,
        /// Target ratio for --ml (fraction of input tokens to keep). Default 0.5.
        #[arg(long, default_value_t = 0.5)]
        ratio: f32,
        /// ONNX model path for `--ml` (requires `--features onnx` build). When
        /// set, the LLMLingua-style token-importance backend runs the model
        /// instead of the heuristic scorer.
        #[arg(long, env = "RTRT_ONNX_MODEL")]
        onnx_model: Option<PathBuf>,
        /// HuggingFace `tokenizer.json` path that matches `--onnx-model`.
        #[arg(long, env = "RTRT_ONNX_TOKENIZER")]
        onnx_tokenizer: Option<PathBuf>,
    },
    /// Report session token usage and Output Optimizer savings.
    #[command(hide = true)]
    Stats,
    /// Show Command Optimizer savings.
    #[command(hide = true)]
    Gain {
        /// Filter rows to this project.
        #[arg(long)]
        project: Option<String>,
        /// Show recent saved runs. Row count is derived from available data.
        #[arg(long)]
        history: bool,
        /// Show daily bucketed totals.
        #[arg(long)]
        daily: bool,
        /// Show weekly bucketed totals.
        #[arg(long)]
        weekly: bool,
        /// Show monthly bucketed totals.
        #[arg(long)]
        monthly: bool,
        /// Show a compact ASCII savings chart over time.
        #[arg(long)]
        graph: bool,
        /// Clear the Command Optimizer stats DB.
        #[arg(long)]
        reset: bool,
        /// Skip confirmation for --reset.
        #[arg(long)]
        yes: bool,
        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: ReportFormatArg,
    },
    /// Filter a command output (read from stdin) for a given command.
    #[command(hide = true)]
    Proxy {
        /// Command being run (e.g. "git status").
        command: String,
    },
    /// Run a shell command through the Command Optimizer.
    #[command(hide = true)]
    ProxyRun {
        /// Print captured output unchanged.
        #[arg(long)]
        raw: bool,
        /// Keep only likely error and warning lines when no command filter matches.
        #[arg(long)]
        errors_only: bool,
        /// Strip ANSI escapes and collapse repeated lines when no command filter matches.
        #[arg(long)]
        ultra_compact: bool,
        /// Command and arguments to run.
        #[arg(num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List available project templates (built-in + custom).
    #[command(hide = true)]
    Templates,
    /// Scaffold a new project from a template.
    #[command(hide = true)]
    New {
        /// Template name (see `rtrt templates`).
        template: String,
        /// Target directory.
        path: PathBuf,
        /// Variables: `--var key=value` (repeatable).
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
        /// Overwrite existing files.
        #[arg(long)]
        overwrite: bool,
        /// Skip running post-init hooks.
        #[arg(long)]
        no_hooks: bool,
    },
    /// Render a template into an existing repository.
    #[command(hide = true)]
    Init {
        /// Template name (defaults to standardization).
        #[arg(long)]
        template: Option<String>,
        /// Existing repository directory (defaults to cwd).
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
        /// Print the file actions without writing.
        #[arg(long)]
        dry_run: bool,
        /// Variables: `--var key=value` (repeatable). Overrides detected values.
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
    },
    /// Migrate an existing repository to the rtrt project standard.
    #[command(hide = true)]
    Migrate {
        /// Template name (defaults to standardization).
        #[arg(long)]
        template: Option<String>,
        /// Existing repository directory (defaults to cwd).
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
        /// Print the full migration plan without writing. This is the default.
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Apply the migration. Without this, migration is dry-run.
        #[arg(long, conflicts_with = "dry_run")]
        apply: bool,
        /// Variables: `--var key=value` (repeatable). Overrides detected values.
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
    },
    /// Inspect and repair the project-standardization lifecycle contract.
    #[command(hide = true)]
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Talk to a chat provider.
    #[command(hide = true)]
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
    /// Invoke a detected local agent or provider through the cross-tool bridge.
    #[command(hide = true)]
    Call {
        /// Target from `rtrt detect`, e.g. claude, codex, ollama, openai.
        target: String,
        /// Invocation mode.
        #[arg(long, value_enum, default_value = "auto")]
        mode: CallModeArg,
        /// Model id used by API calls and templates with `{model}`.
        #[arg(long)]
        model: Option<String>,
        /// Timeout in seconds.
        #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
        /// Output format.
        #[arg(long, value_enum, default_value = "text")]
        format: CallFormatArg,
        /// On a retryable failure (rate-limit / quota / 429 / 5xx / timeout),
        /// fall over to the next ranked target instead of erroring out.
        #[arg(long)]
        failover: bool,
        /// Prompt text. Multiple words are joined with spaces.
        #[arg(num_args = 1.., allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Pick and optionally invoke the cheapest useful route for a prompt.
    #[command(hide = true)]
    Route {
        /// Needed capability.
        #[arg(long, value_enum)]
        capability: Option<RouteCapabilityArg>,
        /// Routing preference.
        #[arg(long, value_enum, default_value = "cheapest")]
        prefer: RoutePreferArg,
        /// Explicit target override.
        #[arg(long)]
        target: Option<String>,
        /// Model id for targets that need or allow a model.
        #[arg(long)]
        model: Option<String>,
        /// Invocation mode.
        #[arg(long, value_enum, default_value = "auto")]
        mode: CallModeArg,
        /// Print the decision, ranked alternatives, and usage/headroom considered.
        #[arg(long)]
        explain: bool,
        /// Print only the decision and do not invoke the target.
        #[arg(long)]
        dry_run: bool,
        /// When invoking, walk the ranked candidate list with automatic
        /// failover on retryable errors. An explicit --target (or the
        /// configured `[providers] active`) stays the primary pick and keeps
        /// its sibling pools and the ranked alternatives behind it. Ignored for
        /// --dry-run / --explain.
        #[arg(long)]
        failover: bool,
        /// Prompt text. Multiple words are joined with spaces.
        #[arg(num_args = 1.., allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Show per-target windowed provider usage and headroom.
    ///
    /// Windowed provider usage (5h / 24h / 7d) and the headroom remaining
    /// against the configured `[limits]` daily caps. Estimated rows (CLI
    /// shell-outs) are marked with `~`.
    #[command(hide = true)]
    Usage {
        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: ReportFormatArg,
    },
    /// Persistent memory operations (SQLite-backed).
    #[command(hide = true)]
    Memory {
        /// Explicit arbitrary legacy/admin SQLite store. Normal commands are
        /// always pinned to the canonical current project.
        #[arg(long, global = true, value_name = "PATH")]
        admin_legacy_store: Option<PathBuf>,
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Run a Criterion benchmark from the workspace and summarise savings.
    #[command(hide = true)]
    Benchmark {
        /// Bench name (default: rtrt-compress `compress_bench`).
        #[arg(long, default_value = "compress_bench")]
        bench: String,
        /// Cargo package to bench (default: rtrt-compress).
        #[arg(long, default_value = "rtrt-compress")]
        package: String,
        /// Extra `cargo bench -- <args>` flags (e.g. `--quick`).
        #[arg(long, value_delimiter = ' ', num_args = 0..)]
        extra: Vec<String>,
    },
    /// Launch the bundled MCP server (passthrough to `rtrt-mcp`).
    #[command(hide = true)]
    Mcp {
        /// Transport. `stdio` (default) for agents; `http` for Streamable HTTP.
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// Bind address for `--transport http`.
        #[arg(long, default_value = "127.0.0.1:7312")]
        bind: String,
        /// HTTP mount path for the MCP endpoint.
        #[arg(long, default_value = "/mcp")]
        path: String,
        /// Explicit arbitrary/global SQLite store (admin/legacy mode only).
        #[arg(long, value_name = "PATH")]
        admin_legacy_memory: Option<PathBuf>,
        /// Allowed Origins (comma-separated) for HTTP transport.
        #[arg(long, env = "RTRT_MCP_ALLOWED_ORIGINS", value_delimiter = ',')]
        allowed_origins: Vec<String>,
        /// Override the discovered `rtrt-mcp` binary path.
        #[arg(long)]
        binary: Option<PathBuf>,
    },
    /// Run the OpenAI-compatible HTTP gateway. Point ANY OpenAI client at
    /// `http://<host>:<port>/v1` and rtrt auto-routes across your detected
    /// providers (use model `auto` / `rtrt/cheapest` / `rtrt/best`, or an
    /// explicit `provider/model`).
    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
    },
    /// Reverse a previous `rtrt setup`.
    ///
    /// Drops the `rtrt` MCP entry from the agent's config; with `--plugin`,
    /// also removes the rtrt hook entries from `~/.claude/settings.json`.
    /// Dry-run by default; pass `--apply` to actually delete.
    #[command(hide = true)]
    Uninstall {
        #[arg(short, long, value_enum)]
        agent: AgentKind,
        #[arg(long)]
        apply: bool,
        /// Also remove the Claude Code hook entries (only with
        /// `--agent claude`).
        #[arg(long)]
        plugin: bool,
    },
    /// Claude Code hook entry points (internal plumbing).
    ///
    /// Used by the `~/.claude/settings.json` entries that `rtrt setup
    /// --plugin --apply` installs. Reads the payload on stdin, strips
    /// control bytes, applies `redact_secrets`, and writes a memory row
    /// with the supplied kind. Exits 0 even on error so a hook never
    /// blocks the host agent.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Print the Output Optimizer statusline badge or rich Claude Code line.
    #[command(hide = true)]
    Statusline {
        /// Force rich mode even when stdin is a TTY.
        #[arg(long)]
        rich: bool,
        /// Override the first rich statusline template.
        #[arg(long)]
        format: Option<String>,
        /// Emit one compact OpenCode status JSON object without reading stdin.
        #[arg(long)]
        opencode: bool,
        /// Working directory supplied by OpenCode.
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,
        /// OpenCode session identifier.
        #[arg(long)]
        session: Option<String>,
        /// Active OpenCode model identifier.
        #[arg(long)]
        model: Option<String>,
        /// Available status-line width in columns.
        #[arg(long, default_value_t = 100)]
        width: usize,
        /// Best-effort collection budget in milliseconds.
        #[arg(long, default_value_t = 120)]
        budget_ms: u64,
        /// Omit Git data even when the width permits it.
        #[arg(long)]
        no_git: bool,
        /// Refresh local snapshots instead of preferring fresh caches.
        #[arg(long)]
        refresh: bool,
    },
    /// Wire RTRT into a popular coding agent's MCP config.
    ///
    /// `--plugin` (Claude only) also merges hook entries into
    /// `~/.claude/settings.json` so every PreToolUse / PostToolUse /
    /// SessionStart etc. auto-captures into the memory store.
    #[command(hide = true)]
    Setup {
        /// Target agent.
        #[arg(short, long, value_enum)]
        agent: AgentKind,
        /// Apply the change. Without this, only a dry-run snippet is printed.
        #[arg(long)]
        apply: bool,
        /// Override the discovered `rtrt-mcp` binary path.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Also install the Claude Code hook entries and status line
        /// (only valid with `--agent claude`).
        #[arg(long)]
        plugin: bool,
        /// Enable strict Linux sandboxing for OpenCode's built-in shell tool.
        #[arg(long, conflicts_with = "no_sandbox")]
        sandbox: bool,
        /// Remove execution agents, then restore RTRT-owned shell; recreated
        /// agents deny every Bash pattern.
        #[arg(long, conflicts_with = "sandbox")]
        no_sandbox: bool,
        /// Configure only machine-global strict OpenCode shell ownership.
        /// Never discovers or authorizes the current checkout.
        #[arg(long)]
        machine_only: bool,
    },
    /// Run `rtrt-dashboard` as a background OS service.
    ///
    /// Starts on login and restarts on crash (systemd --user on Linux,
    /// launchd on macOS). Dry-run by default; pass `--apply`.
    #[command(hide = true)]
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Security scanning and profile management.
    #[command(hide = true)]
    Security {
        #[command(subcommand)]
        cmd: security::SecurityCmd,
    },
    /// Extract top-level signatures from source via tree-sitter (drops bodies).
    #[command(hide = true)]
    Signatures {
        /// Language. Currently: `rust`.
        #[arg(long, default_value = "rust")]
        lang: String,
    },
    /// Versioned prompt registry (file-backed under ~/.rtrt/prompts/).
    #[command(hide = true)]
    Prompt {
        #[command(subcommand)]
        cmd: PromptCmd,
    },
    /// Walk a directory and emit a tree-sitter signature map of every Rust file.
    #[command(hide = true)]
    RepoMap {
        /// Root directory to walk.
        root: PathBuf,
        /// Skip files larger than this many bytes.
        #[arg(long, default_value_t = 524_288)]
        max_bytes: u64,
        /// File-name suffix filter. Empty = auto-detect every supported
        /// language (.rs / .py / .ts / .tsx). Set to e.g. `.rs` to restrict.
        #[arg(long, default_value = "")]
        ext: String,
    },
    /// Build a compressed git-state context for the LLM.
    #[command(hide = true)]
    Context {
        #[command(subcommand)]
        cmd: ContextCmd,
    },
    /// Run a command, capture failures, then ask an LLM for a fix suggestion.
    #[command(hide = true)]
    Diagnose {
        /// Command + args.
        #[arg(num_args = 1..)]
        argv: Vec<String>,
        /// Provider for the LLM diagnosis.
        #[arg(short, long, value_enum)]
        provider: ProviderArg,
        /// Model id.
        #[arg(short, long)]
        model: String,
        /// Override the base URL for openai-compat providers.
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        /// Context lines kept around each captured error.
        #[arg(long, default_value_t = 3)]
        context: usize,
    },
    /// Run a command, capture stdout+stderr, and filter to errors/warnings only.
    #[command(hide = true)]
    Run {
        /// Command + args. Quote spaces.
        #[arg(num_args = 1..)]
        argv: Vec<String>,
        /// Lines of context to keep around each match.
        #[arg(long, default_value_t = 1)]
        context: usize,
        /// Apply the ultra-compact pass (strip ANSI + collapse runs) instead of
        /// the errors-only filter.
        #[arg(long)]
        compact: bool,
        /// Exit code: 0 even when the command failed (default). Pass to surface
        /// the underlying command's exit code instead.
        #[arg(long)]
        passthrough_status: bool,
    },
    /// Fetch library docs from context7 (`/owner/repo`, optional --topic).
    #[command(hide = true)]
    Docs {
        /// Library id as `<owner>/<repo>` (e.g. `facebook/react`).
        library: String,
        /// Optional topic filter (e.g. `hooks`).
        #[arg(long)]
        topic: Option<String>,
        /// Override the context7 base URL (useful for self-hosting).
        #[arg(long, default_value = "https://context7.com/api/v1")]
        base_url: String,
    },
    /// Discover commands that can use the Command Optimizer.
    #[command(hide = true)]
    Discover {
        /// Filter transcript commands to this project.
        #[arg(long)]
        project: Option<String>,
        /// Scan every project instead of the current project.
        #[arg(long, conflicts_with = "project")]
        all: bool,
        /// Include commands on or after this date or timestamp.
        #[arg(long)]
        since: Option<String>,
        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: ReportFormatArg,
    },
    /// Detect local AI agents, runtimes, provider APIs, and MCP servers.
    #[command(hide = true)]
    Detect {
        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: DetectFormatArg,
        /// Restrict results to one tool kind.
        #[arg(long, value_enum)]
        kind: Option<DetectKindArg>,
        /// Show only installed/configured entries.
        #[arg(long)]
        installed_only: bool,
        /// Show only enabled entries.
        #[arg(long)]
        enabled_only: bool,
    },
    /// Show RTRT version + crate manifest.
    #[command(hide = true)]
    Info,
    /// Manage the global config file (`~/.rtrt/config.toml`).
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// One-shot health check for the rtrt install.
    ///
    /// Probes PATH binary versions, Claude Code integration (hooks / MCP /
    /// statusline), the memory store, detected agents/providers, the
    /// dashboard service, and the provider-usage ledger. Every row is a
    /// real local probe; exits non-zero only when a critical check fails.
    #[command(hide = true)]
    Doctor {
        /// Emit the report as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OpenCodeAction {
    /// Inspect or migrate the global OpenCode session graph.
    Sessions {
        #[command(subcommand)]
        command: OpenCodeSessionsAction,
    },
    /// Report known global prompt-history files; never writes.
    HistoryStatus,
    /// Quarantine known global prompt-history files. Dry-run by default.
    HistoryQuarantine {
        /// Rename files to timestamp-free `.rtrt-quarantine` siblings.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
enum OpenCodeSessionsAction {
    /// Probe and report; never writes.
    Status,
    /// Plan migration; never writes.
    DryRun,
    /// Migrate every attributable graph and archive the remainder.
    Apply,
}

#[derive(Debug, Subcommand)]
enum GatewayCmd {
    /// Start the OpenAI-compatible HTTP server. Binds loopback by default.
    Serve {
        /// Bind port.
        #[arg(long, default_value_t = DEFAULT_GATEWAY_PORT)]
        port: u16,
        /// Bind host. Keep loopback unless you also set a bearer token.
        #[arg(long, default_value = DEFAULT_GATEWAY_HOST)]
        host: String,
        /// Optional bearer token required on `/v1/*`. Reads `RTRT_GATEWAY_TOKEN`
        /// by default. `/healthz` is always open.
        #[arg(long, env = "RTRT_GATEWAY_TOKEN")]
        token: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Write a commented starter config to `~/.rtrt/config.toml`
    /// (or `$RTRT_CONFIG`). Refuses to clobber an existing file unless
    /// `--force`.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Print the resolved config path and whether it exists.
    Path,
}

#[derive(Debug, Subcommand)]
enum ProjectCmd {
    /// Report project contract, agents, hooks, status line, and memory reachability.
    Status {
        /// Repository root to inspect. Defaults to the current directory.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
    },
    /// Run status plus deeper lifecycle consistency checks.
    Health {
        /// Repository root to inspect. Defaults to the current directory.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
    },
    /// Append missing managed sections and install missing project agents.
    Repair {
        /// Repository root to repair. Defaults to the current directory.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
        /// Preview the repair actions without writing files.
        #[arg(long)]
        dry_run: bool,
    },
    /// One-command project integration (alias for `rtrt migrate`): render the
    /// project contract, activate rtrt features to canonical settings, and
    /// audit whole-project consistency. Dry-run by default; `--apply` to write.
    Refresh {
        /// Repository root to refresh. Defaults to the current directory.
        #[arg(long, value_name = "DIR")]
        path: Option<PathBuf>,
        /// Template name (defaults to standardization).
        #[arg(long)]
        template: Option<String>,
        /// Apply the changes. Without this, refresh is dry-run.
        #[arg(long)]
        apply: bool,
        /// Variables: `--var key=value` (repeatable). Overrides detected values.
        #[arg(long = "var", value_parser = parse_var)]
        vars: Vec<(String, String)>,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCmd {
    /// Open this project's dashboard and authenticate this browser tab.
    Open {
        /// Print a URL containing only the 60-second bootstrap credential.
        #[arg(long)]
        print_bootstrap: bool,
    },
    /// Write + enable the OS service for `rtrt-dashboard`.
    Install {
        /// Apply the change. Without this, only a dry-run is printed.
        #[arg(long)]
        apply: bool,
        /// Override the discovered `rtrt-dashboard` binary path.
        #[arg(long)]
        binary: Option<PathBuf>,
    },
    /// Stop + remove the OS service.
    Uninstall {
        #[arg(long)]
        apply: bool,
    },
    /// Show the service status.
    Status,
}

#[derive(Debug, Subcommand)]
enum HookCmd {
    /// Save the stdin payload as a memory row tagged with `kind`. Intended
    /// to be the entry point for `~/.claude/settings.json` hook commands.
    Capture {
        /// Memory `kind` to tag the row with — e.g. `pre-tool-use`,
        /// `post-tool-use`, `session-start`. Free-form.
        kind: String,
        /// Project bucket. Defaults to `$RTRT_PROJECT` or the git
        /// repository root of the current working directory.
        #[arg(long)]
        project: Option<String>,
        /// Memory store path. Defaults to `~/.rtrt/memory.sqlite` so every
        /// hook fire lands in the same SQLite file as the MCP server.
        #[arg(long, env = "RTRT_MEMORY_PATH")]
        store: Option<PathBuf>,
    },
    /// Persist an OpenCode/RTRT parent invocation against the Claude child
    /// session in the SessionStart payload. No-op outside a propagated call.
    Provenance {
        #[arg(long, env = "RTRT_MEMORY_PATH")]
        store: Option<PathBuf>,
        /// Setup-owner marker used to uninstall the correct hook entry.
        #[arg(long, hide = true)]
        owner: Option<String>,
    },
    /// Update or reinforce Output Optimizer terse mode on user prompts.
    Style,
    /// Inject Output Optimizer terse-mode rules at session start.
    StyleInject,
    /// Print the Output Optimizer statusline badge.
    Statusline,
    /// Rewrite simple Bash commands so Claude Code can run them through the
    /// Command Optimizer.
    ProxyRewrite,
    /// Recall memory relevant to the stdin prompt and print it to stdout as
    /// a context block. Wired onto `UserPromptSubmit` so Claude Code injects
    /// the project's relevant history into the model's context automatically
    /// — no manual `memory_recall` call needed.
    Recall {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH")]
        store: Option<PathBuf>,
        /// Max memories to inject.
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
    /// Compress old memory rows for the project via the configured LLM,
    /// in place. Wired onto `SessionEnd` so compression runs automatically
    /// without a long-lived dashboard daemon. No-op unless
    /// `RTRT_AUTO_COMPRESS_LLM=1` and a provider is reachable.
    Compress {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH")]
        store: Option<PathBuf>,
    },
    /// Inject the project's most-important memories into the model context at
    /// session start. Wired onto `SessionStart` so background knowledge is
    /// available from turn 1 without waiting for a prompt. Reads from the
    /// same store as `hook capture` / `hook recall`.
    SessionInject {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH")]
        store: Option<PathBuf>,
        /// Number of memories to surface.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCmd {
    /// Save a raw memory record (BM25-indexed). Body from arg or stdin.
    Save {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "note")]
        kind: String,
        body: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Metadata pair `key=value` (repeatable) — wires into qdrant-style
        /// payload filtering on recall.
        #[arg(long = "meta", value_parser = parse_var)]
        meta: Vec<(String, String)>,
    },
    /// Letta-style memory blocks (persona / human / context slots).
    Blocks {
        #[command(subcommand)]
        cmd: BlockCmd,
    },
    /// Recall memories by BM25 (FTS5).
    Recall {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long)]
        store: Option<PathBuf>,
        /// qdrant-style payload filter (e.g. `source=claude,topic~^auth`).
        #[arg(long)]
        filter: Option<String>,
    },
    /// Export every memory row in a project to JSON Lines (stdout if `--out` omitted).
    Export {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Destination file. `-` (or omit) writes to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import JSON Lines emitted by `rtrt memory export` (stdin if `--in` omitted).
    Import {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Source file. `-` (or omit) reads from stdin.
        #[arg(long = "in")]
        input: Option<PathBuf>,
    },
    /// Re-embed every memory row whose stored `embeddings.model` differs
    /// from the configured / `--model` embedder. Needed when switching the
    /// embedding model (e.g. `nomic-embed-text` → `bge-m3`): old cosine
    /// vectors live in a different space, so recall must filter on the
    /// model name and the old rows must be overwritten with new vectors.
    /// Pulls `[unembedded backlog ∪ stale-model rows]` in bounded database
    /// batches, embeds them through the configured embedder, and writes via
    /// `INSERT OR REPLACE` so resuming after an interrupt skips nothing
    /// already upgraded. Stays a no-op when no row is stale.
    Reembed {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Limit the sweep to one project. Omit (or pass `--all`) to sweep
        /// the whole store in `id ASC` order.
        #[arg(long)]
        project: Option<String>,
        #[arg(long, conflicts_with = "project")]
        all: bool,
        /// Override the configured embedder's model id. Defaults to
        /// `embeddings.effective_model()` from `~/.rtrt/config.toml`.
        #[arg(long, env = "RTRT_EMBED_MODEL")]
        model: Option<String>,
        /// Override the configured Ollama base URL.
        #[arg(long, env = "RTRT_EMBED_BASE_URL")]
        base_url: Option<String>,
        /// Texts sent in each Ollama `/api/embed` request.
        #[arg(
            long,
            default_value_t = 32,
            value_parser = parse_embed_batch_size
        )]
        batch: usize,
        /// Concurrent Ollama batch requests. DB writes remain single-threaded.
        #[arg(long, default_value_t = 8, value_parser = parse_worker_count)]
        workers: usize,
        /// Print what would be re-embedded and exit (no writes, no embeds).
        #[arg(long)]
        dry_run: bool,
        /// Exit non-zero when stale rows remain so CI can gate on "store is
        /// fully migrated" without embedding or writing anything.
        #[arg(long, conflicts_with = "dry_run")]
        probe: bool,
    },
    /// Extract atomic facts from a passage via LLM and save each.
    Extract {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "note")]
        kind: String,
        body: Option<String>,
        #[arg(short, long, value_enum)]
        provider: ProviderArg,
        #[arg(short, long)]
        model: String,
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Compress old memories — keep the most recent N, summarise the rest.
    Compress {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        keep: usize,
        #[arg(short, long, value_enum)]
        provider: ProviderArg,
        #[arg(short, long)]
        model: String,
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Copy attributable rows from a legacy mixed DB into this project's
    /// isolated store. Dry-run unless `--apply`; source remains untouched.
    LegacyIsolate {
        #[arg(long, value_name = "PATH")]
        source: PathBuf,
        #[arg(long)]
        apply: bool,
        /// Explicitly claim rows carrying this project's ambiguous basename.
        #[arg(long, requires = "accept_mixed_history")]
        claim_basename: bool,
        /// Acknowledge historical basename mixing cannot be disentangled.
        #[arg(long)]
        accept_mixed_history: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ContextCmd {
    /// `git status` filtered through `rtrt-proxy`.
    Status {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// `git diff [base]` filtered through `rtrt-proxy`.
    Diff {
        /// Base ref. Empty = working tree vs HEAD.
        base: Option<String>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// `git log -<n>` filtered through `rtrt-proxy`.
    Log {
        #[arg(short, long, default_value_t = 20)]
        count: u32,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PromptCmd {
    /// Save a new version of a named prompt. Body from arg or stdin.
    Save {
        name: String,
        body: Option<String>,
        #[arg(long = "meta", value_parser = parse_var)]
        meta: Vec<(String, String)>,
        #[arg(long, default_value = ".rtrt/prompts")]
        registry: PathBuf,
    },
    /// Fetch a prompt (latest unless --version given).
    Get {
        name: String,
        #[arg(long)]
        version: Option<u32>,
        #[arg(long, default_value = ".rtrt/prompts")]
        registry: PathBuf,
    },
    /// List every registered prompt name.
    List {
        #[arg(long, default_value = ".rtrt/prompts")]
        registry: PathBuf,
    },
    /// List every version of `name`.
    Versions {
        name: String,
        #[arg(long, default_value = ".rtrt/prompts")]
        registry: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCmd {
    /// Send a single chat turn and print the response.
    Chat {
        /// Prompt text (also reads stdin if `-` or empty).
        prompt: Option<String>,
        /// Model id (e.g. `claude-haiku-4-5`, `gpt-5.4-mini`).
        #[arg(short, long)]
        model: String,
        /// Provider override (auto-detected from model when omitted).
        #[arg(short, long, value_enum)]
        provider: Option<ProviderArg>,
        /// Stream tokens to stdout as they arrive.
        #[arg(long)]
        stream: bool,
        /// Custom base URL for `--provider openai-compat`.
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        /// Override the default 1024-token cap.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// Optional system prompt.
        #[arg(long)]
        system: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum LevelArg {
    Lite,
    Full,
    Ultra,
    Extreme,
}

impl From<LevelArg> for CompressionLevel {
    fn from(l: LevelArg) -> Self {
        match l {
            LevelArg::Lite => CompressionLevel::Lite,
            LevelArg::Full => CompressionLevel::Full,
            LevelArg::Ultra => CompressionLevel::Ultra,
            LevelArg::Extreme => CompressionLevel::Extreme,
        }
    }
}

#[derive(Debug, Subcommand)]
enum BlockCmd {
    /// Upsert a block (overwrites any existing slot with the same name).
    Set {
        #[arg(long)]
        project: Option<String>,
        name: String,
        body: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Print one block.
    Get {
        #[arg(long)]
        project: Option<String>,
        name: String,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// List every block in a project.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ProviderArg {
    Anthropic,
    Openai,
    OpenaiCompat,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum FormatArg {
    Plain,
    Markdown,
    Xml,
    Json,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ReportFormatArg {
    Json,
    Table,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CallFormatArg {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CallModeArg {
    Cli,
    Api,
    Auto,
}

impl From<CallModeArg> for InvokeMode {
    fn from(mode: CallModeArg) -> Self {
        match mode {
            CallModeArg::Cli => InvokeMode::Cli,
            CallModeArg::Api => InvokeMode::Api,
            CallModeArg::Auto => InvokeMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RouteCapabilityArg {
    Code,
    Reasoning,
    Vision,
    Embed,
    Cheap,
}

impl From<RouteCapabilityArg> for Capability {
    fn from(capability: RouteCapabilityArg) -> Self {
        match capability {
            RouteCapabilityArg::Code => Capability::Code,
            RouteCapabilityArg::Reasoning => Capability::Reasoning,
            RouteCapabilityArg::Vision => Capability::Vision,
            RouteCapabilityArg::Embed => Capability::Embed,
            RouteCapabilityArg::Cheap => Capability::CheapBulk,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RoutePreferArg {
    Cheapest,
    Quality,
    Local,
}

impl From<RoutePreferArg> for Prefer {
    fn from(prefer: RoutePreferArg) -> Self {
        match prefer {
            RoutePreferArg::Cheapest => Prefer::Cheapest,
            RoutePreferArg::Quality => Prefer::Quality,
            RoutePreferArg::Local => Prefer::Local,
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum DetectFormatArg {
    Table,
    Json,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum DetectKindArg {
    CodingAgent,
    LocalRuntime,
    ProviderApi,
    McpServer,
}

impl From<DetectKindArg> for ToolKind {
    fn from(kind: DetectKindArg) -> Self {
        match kind {
            DetectKindArg::CodingAgent => ToolKind::CodingAgent,
            DetectKindArg::LocalRuntime => ToolKind::LocalRuntime,
            DetectKindArg::ProviderApi => ToolKind::ProviderApi,
            DetectKindArg::McpServer => ToolKind::McpServer,
        }
    }
}

const PROXY_RUN_ERROR_CONTEXT_LINES: usize = 1;
const EXEC_FAILURE_EXIT_CODE: i32 = 1;
const STDERR_TO_STDOUT_REDIRECT: &str = " 2>&1";
const PROXY_RUN_PREFIX: &str = "rtrt proxy-run";
const LEGACY_PROXY_PREFIX: &str = concat!("r", "t", "k", " ");
const SHELL_COMPLEX_MARKERS: &[&str] = &["|", "&&", "||", ";", "$(", "`", ">", "<", "\n"];
const KNOWN_SHRINKABLE_COMMANDS: &[&str] =
    &["git", "cargo", "docker", "kubectl", "npm", "yarn", "pnpm"];
const DETECT_NAME_WIDTH: usize = 16;
const DETECT_INSTALLED_WIDTH: usize = 9;
const DETECT_VERSION_WIDTH: usize = 18;
const DETECT_MODES_WIDTH: usize = 9;
const DETECT_COST_WIDTH: usize = 17;
const DETECT_ENABLED_WIDTH: usize = 7;
const DETECT_DETAIL_WIDTH: usize = 72;
const DEFAULT_INIT_TEMPLATE: &str = "standardization";
// Ignore only the per-project runtime artifacts under `.rtrt/`; keep
// `.rtrt/config.toml` (the per-project customization override) tracked so it
// travels with the repo for the whole team.
const MIGRATE_GITIGNORE_ENTRIES: &[&str] = &[
    ".rtrt/*.sqlite",
    ".rtrt/*.sqlite-journal",
    ".rtrt/*.sqlite-wal",
    ".rtrt/*.sqlite-shm",
    ".claude/settings.local.json",
];
const PROJECT_STATE_WIDTH: usize = 4;
const PROJECT_CHECK_WIDTH: usize = 18;
#[cfg(unix)]
const UNIX_EXECUTE_BITS: u32 = 0o111;
const DETECT_KIND_ORDER: &[ToolKind] = &[
    ToolKind::CodingAgent,
    ToolKind::LocalRuntime,
    ToolKind::ProviderApi,
    ToolKind::McpServer,
];

impl From<FormatArg> for rtrt_compress::OutputFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Plain => rtrt_compress::OutputFormat::Plain,
            FormatArg::Markdown => rtrt_compress::OutputFormat::Markdown,
            FormatArg::Xml => rtrt_compress::OutputFormat::Xml,
            FormatArg::Json => rtrt_compress::OutputFormat::Json,
        }
    }
}

fn parse_var(s: &str) -> std::result::Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got `{s}`"))?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

fn run_init(
    template: Option<String>,
    path: Option<PathBuf>,
    force: bool,
    dry_run: bool,
    vars: Vec<(String, String)>,
) -> Result<()> {
    let target = match path {
        Some(path) => path,
        None => std::env::current_dir().context("resolve current directory")?,
    };
    let target = std::fs::canonicalize(&target)
        .with_context(|| format!("target path does not exist: {}", target.display()))?;
    if !target.is_dir() {
        bail!("target path is not a directory: {}", target.display());
    }

    let template_name = template.unwrap_or_else(|| DEFAULT_INIT_TEMPLATE.to_string());
    let tmpl = rtrt_templates::find(&template_name)
        .with_context(|| format!("unknown template: {template_name}"))?;
    validate_init_template_paths(&tmpl)?;

    let mut map = detect_init_vars(&target);
    for (key, value) in vars {
        map.insert(key, value);
    }

    let plan = rtrt_templates::render::plan(&tmpl, &target, map)?;
    let mut written = 0usize;
    let mut skipped = 0usize;

    println!("init template {} -> {}", tmpl.name, plan.root.display());
    for file in &plan.files {
        let rel = safe_rendered_relative_path(&plan.root, &file.path)?;
        let rel_display = rel.display();
        let exists = file.path.exists();
        if exists && !force {
            skipped += 1;
            if dry_run {
                println!("would skip {rel_display}");
            } else {
                println!("skipped {rel_display}");
            }
            continue;
        }

        written += 1;
        if dry_run {
            let action = if exists {
                "would overwrite"
            } else {
                "would write"
            };
            println!("{action} {rel_display}");
            continue;
        }

        if let Some(parent) = file.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create directory: {}", parent.display()))?;
        }
        std::fs::write(&file.path, &file.content)
            .with_context(|| format!("write file: {}", file.path.display()))?;
        set_executable_if_requested(&file.path, file.executable)?;

        let action = if exists { "overwrote" } else { "wrote" };
        println!("{action} {rel_display}");
    }

    let verb = if dry_run { "would write" } else { "written" };
    println!("init complete: {written} {verb}, {skipped} skipped");
    Ok(())
}

/// rtrt-owned settings keys whose canonical value lives in the global
/// `~/.claude/settings.json`. A project-level `<repo>/.claude/settings.json`
/// that re-declares them shadows the global rtrt config (Claude Code merges
/// project over user), so migrate strips them and lets the project defer to
/// the global base kernel.
const RTRT_OWNED_SETTINGS_KEYS: &[&str] = &["statusLine"];

/// Detect rtrt-owned keys declared at the project level that would shadow the
/// global base kernel. Returns the settings path and the offending key list.
fn project_settings_override(root: &Path) -> Option<(PathBuf, Vec<String>)> {
    let path = root.join(".claude").join("settings.json");
    let raw = std::fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let obj = value.as_object()?;
    let keys: Vec<String> = RTRT_OWNED_SETTINGS_KEYS
        .iter()
        .filter(|k| obj.contains_key(**k))
        .map(|k| (*k).to_string())
        .collect();
    if keys.is_empty() {
        None
    } else {
        Some((path, keys))
    }
}

/// Remove the rtrt-owned keys from a project-level settings file so the project
/// defers to the global base kernel. Writes a `.bak` of the original first.
fn strip_project_settings_override(path: &Path, keys: &[String]) -> Result<()> {
    let raw = std::fs::read_to_string(path).context("read project settings.json")?;
    std::fs::write(path.with_extension("json.bak"), &raw).context("back up project settings")?;
    let mut value: serde_json::Value =
        serde_json::from_str(&raw).context("parse project settings.json")?;
    if let Some(obj) = value.as_object_mut() {
        for key in keys {
            obj.remove(key);
        }
    }
    let pretty = serde_json::to_string_pretty(&value).context("serialize project settings")?;
    std::fs::write(path, format!("{pretty}\n")).context("write project settings.json")?;
    Ok(())
}

fn run_migrate(
    template: Option<String>,
    path: Option<PathBuf>,
    apply: bool,
    vars: Vec<(String, String)>,
) -> Result<()> {
    let root = resolve_project_path(path)?;
    let template_name = template.unwrap_or_else(|| DEFAULT_INIT_TEMPLATE.to_string());
    let tmpl = rtrt_templates::project::contract_template(&template_name)
        .with_context(|| format!("unknown template: {template_name}"))?;
    validate_contract_template_paths(&tmpl)?;

    let mut map = detect_init_vars(&root);
    for (key, value) in vars {
        map.insert(key, value);
    }

    let dry_run = !apply;
    let repair =
        rtrt_templates::project::plan_repair_with_vars(&root, &template_name, map.clone())?;
    let retirement = rtrt_templates::project::plan_legacy_orchestration_retirement(&root)?;
    let gitignore_missing = missing_gitignore_entries(&root)?;
    let mcp_binary = resolve_mcp_binary();

    println!("rtrt migrate template {} -> {}", tmpl.name, root.display());
    println!(
        "mode: {}",
        if dry_run {
            "dry-run (pass --apply to write)"
        } else {
            "apply"
        }
    );
    print_migrate_vars(&map);
    println!("3-step plan:");
    println!("1. Render template project contract");
    println!("2. Activate rtrt features to canonical settings");
    println!("3. Audit whole-project consistency");

    println!("\nSTEP 1 — Render template project contract");
    if repair.actions.is_empty() && retirement.actions.is_empty() {
        println!("skip: CLAUDE.md managed sections and project agents already present");
    } else {
        for action in &repair.actions {
            print_repair_action(action, dry_run);
        }
        for action in &retirement.actions {
            print_legacy_retirement_action(action, dry_run);
        }
    }
    if apply {
        backup_repo_files_for_repair(&repair)?;
        rtrt_templates::project::apply_legacy_orchestration_retirement(&retirement)?;
        rtrt_templates::project::apply_repair(&repair)?;
    }

    println!("\nSTEP 2 — Activate rtrt features to canonical settings");
    setup::run(SetupPlan {
        agent: AgentKind::Claude,
        apply,
        memory_path: None,
        binary: mcp_binary,
        plugin: true,
        sandbox: false,
        no_sandbox: false,
        machine_only: false,
    })?;

    println!("\nSTEP 3 — Audit whole-project consistency");
    match project_settings_override(&root) {
        Some((settings_path, keys)) if dry_run => {
            println!(
                "[dry-run] would remove project-level {} override in {} (defer to global rtrt)",
                keys.join(", "),
                settings_path.display()
            );
        }
        Some((settings_path, keys)) => {
            strip_project_settings_override(&settings_path, &keys)?;
            println!(
                "removed project-level {} override in {} -> defers to global rtrt (backup .bak)",
                keys.join(", "),
                settings_path.display()
            );
        }
        None => {
            println!("project settings: no rtrt-owned key shadows the global base kernel");
        }
    }
    if gitignore_missing.is_empty() {
        println!("gitignore: rtrt/agent local state entries present");
    } else if dry_run {
        println!(
            "[dry-run] would update .gitignore with {}",
            gitignore_missing.join(", ")
        );
    } else {
        apply_gitignore_entries(&root, &gitignore_missing)?;
        println!("updated .gitignore with {}", gitignore_missing.join(", "));
    }

    if dry_run {
        println!(
            "[dry-run] would ensure memory DB is reachable at {}",
            rtrt_core::default_memory_store_path().display()
        );
    } else {
        let path = rtrt_core::default_memory_store_path();
        let _store = MemoryStore::open(&path).map_err(anyhow::Error::from)?;
        println!("memory DB reachable at {}", path.display());
    }

    let inspection =
        rtrt_templates::project::inspect_project_with_vars(&root, &template_name, map)?;
    print_migrate_audit(&inspection, &root);
    Ok(())
}

fn print_migrate_vars(vars: &BTreeMap<String, String>) {
    let keys = ["project_name", "language", "framework"];
    let rendered = keys
        .into_iter()
        .filter_map(|key| vars.get(key).map(|value| format!("{key}={value}")))
        .collect::<Vec<_>>();
    if !rendered.is_empty() {
        println!("vars: {}", rendered.join(", "));
    }
}

fn validate_contract_template_paths(template: &rtrt_templates::Template) -> Result<()> {
    validate_init_template_paths(template)?;
    for file in &template.files {
        let path = Path::new(&file.path);
        if path != Path::new(rtrt_templates::project::CONTRACT_PATH)
            && !path.starts_with(rtrt_templates::project::AGENTS_DIR)
        {
            bail!(
                "template {} contains non-contract file: {}",
                template.name,
                file.path
            );
        }
    }
    Ok(())
}

fn resolve_mcp_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rtrt-mcp")))
        .unwrap_or_else(|| PathBuf::from("rtrt-mcp"))
}

fn backup_repo_files_for_repair(plan: &rtrt_templates::project::ProjectRepairPlan) -> Result<()> {
    let edits_contract = plan.actions.iter().any(|action| {
        matches!(
            action,
            rtrt_templates::project::RepairAction::AppendSection { .. }
        )
    });
    if edits_contract {
        backup_repo_file(&plan.root.join(rtrt_templates::project::CONTRACT_PATH))?;
    }
    Ok(())
}

fn missing_gitignore_entries(root: &Path) -> Result<Vec<String>> {
    let path = root.join(".gitignore");
    let raw = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    let present = raw
        .lines()
        .map(|line| line.trim())
        .collect::<std::collections::BTreeSet<_>>();
    Ok(MIGRATE_GITIGNORE_ENTRIES
        .iter()
        .filter(|entry| !present.contains(**entry))
        .map(|entry| (*entry).to_string())
        .collect())
}

fn apply_gitignore_entries(root: &Path, missing: &[String]) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let path = root.join(".gitignore");
    if path.exists() {
        backup_repo_file(&path)?;
    }
    let mut out = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("# rtrt local state\n");
    for entry in missing {
        out.push_str(entry);
        out.push('\n');
    }
    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn backup_repo_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let bak = path.with_extension({
        let mut ext = path
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !ext.is_empty() {
            ext.push('.');
        }
        ext.push_str("bak");
        ext
    });
    if !bak.exists() {
        std::fs::copy(path, &bak)
            .with_context(|| format!("backup {} to {}", path.display(), bak.display()))?;
    }
    Ok(())
}

fn print_migrate_audit(inspection: &rtrt_templates::project::ProjectInspection, root: &Path) {
    let settings = claude_settings_status(true);
    let memory = memory_reachable_status(true);
    let gitignore_missing = missing_gitignore_entries(root).unwrap_or_default();
    println!("audit:");
    print_project_rows(&[
        sections_check_row(inspection, true),
        agents_check_row(inspection, true),
        stale_sections_check_row(inspection),
        duplicate_sections_check_row(inspection),
        ProjectCheckRow {
            state: settings.hooks_state,
            check: "hooks",
            detail: settings.hooks_detail.clone(),
        },
        ProjectCheckRow {
            state: settings.statusline_state,
            check: "statusLine",
            detail: settings.statusline_detail.clone(),
        },
        ProjectCheckRow {
            state: memory.0,
            check: "memory DB",
            detail: memory.1.clone(),
        },
        ProjectCheckRow {
            state: if gitignore_missing.is_empty() {
                ProjectCheckState::Pass
            } else {
                ProjectCheckState::Warn
            },
            check: ".gitignore",
            detail: if gitignore_missing.is_empty() {
                "rtrt/agent local state ignored".into()
            } else {
                format!("missing {}", gitignore_missing.join(", "))
            },
        },
    ]);
    let blockers = migrate_manual_followups(inspection, &settings, &memory, &gitignore_missing);
    if blockers.is_empty() {
        println!("manual follow-up: none");
    } else {
        println!("manual follow-up:");
        for blocker in blockers {
            println!("- {blocker}");
        }
    }
}

fn migrate_manual_followups(
    inspection: &rtrt_templates::project::ProjectInspection,
    settings: &ClaudeSettingsStatus,
    memory: &(ProjectCheckState, String),
    gitignore_missing: &[String],
) -> Vec<String> {
    let mut items = Vec::new();
    let stale = inspection
        .sections
        .iter()
        .filter(|section| section.stale)
        .map(|section| section.number.to_string())
        .collect::<Vec<_>>();
    if !stale.is_empty() {
        items.push(format!(
            "CLAUDE.md section titles differ from template: {}",
            stale.join(",")
        ));
    }
    if !inspection.duplicate_sections.is_empty() {
        items.push(format!(
            "duplicate managed CLAUDE.md sections require manual merge: {}",
            inspection
                .duplicate_sections
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if settings.hooks_state != ProjectCheckState::Pass {
        items.push(format!(
            "rtrt hooks not confirmed: {}",
            settings.hooks_detail
        ));
    }
    if settings.statusline_state != ProjectCheckState::Pass {
        items.push(format!(
            "rtrt statusLine not confirmed: {}",
            settings.statusline_detail
        ));
    }
    if memory.0 != ProjectCheckState::Pass {
        items.push(format!("memory DB not reachable: {}", memory.1));
    }
    if !gitignore_missing.is_empty() {
        items.push(format!(
            ".gitignore still missing {}",
            gitignore_missing.join(", ")
        ));
    }
    items
}

fn run_project(cmd: ProjectCmd) -> Result<()> {
    match cmd {
        ProjectCmd::Status { path } => {
            let root = resolve_project_path(path)?;
            print_project_report(&root, false)
        }
        ProjectCmd::Health { path } => {
            let root = resolve_project_path(path)?;
            print_project_report(&root, true)
        }
        ProjectCmd::Repair { path, dry_run } => {
            let root = resolve_project_path(path)?;
            run_project_repair(&root, dry_run)
        }
        ProjectCmd::Refresh {
            path,
            template,
            apply,
            vars,
        } => run_migrate(template, path, apply, vars),
    }
}

fn resolve_project_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let raw = match path {
        Some(path) => path,
        None => std::env::current_dir().context("resolve current directory")?,
    };
    rtrt_templates::project::validate_project_path(&raw).map_err(anyhow::Error::from)
}

struct ProjectCheckRow {
    state: ProjectCheckState,
    check: &'static str,
    detail: String,
}

fn print_project_report(root: &Path, health: bool) -> Result<()> {
    let inspection = rtrt_templates::project::inspect_project(root)?;
    let settings = claude_settings_status(health);
    let memory = memory_reachable_status(health);
    let mut rows = Vec::new();
    rows.push(ProjectCheckRow {
        state: ProjectCheckState::Pass,
        check: "root",
        detail: inspection.root.display().to_string(),
    });
    rows.push(ProjectCheckRow {
        state: if inspection.contract_present {
            ProjectCheckState::Pass
        } else {
            ProjectCheckState::Warn
        },
        check: "CLAUDE.md",
        detail: if inspection.contract_present {
            "present".into()
        } else {
            "missing".into()
        },
    });
    rows.push(sections_check_row(&inspection, health));
    rows.push(agents_check_row(&inspection, health));
    rows.push(ProjectCheckRow {
        state: if inspection.present_agents.is_empty() {
            ProjectCheckState::Warn
        } else {
            ProjectCheckState::Pass
        },
        check: "agent files",
        detail: if inspection.present_agents.is_empty() {
            "none present".into()
        } else {
            inspection.present_agents.join(", ")
        },
    });
    rows.push(ProjectCheckRow {
        state: settings.hooks_state,
        check: "hooks",
        detail: settings.hooks_detail,
    });
    rows.push(ProjectCheckRow {
        state: settings.statusline_state,
        check: "statusLine",
        detail: settings.statusline_detail,
    });
    rows.push(ProjectCheckRow {
        state: memory.0,
        check: "memory DB",
        detail: memory.1,
    });

    if health {
        rows.push(stale_sections_check_row(&inspection));
        rows.push(extra_sections_check_row(&inspection));
        rows.push(duplicate_sections_check_row(&inspection));
        rows.push(ProjectCheckRow {
            state: if root.join(".git").exists() {
                ProjectCheckState::Pass
            } else {
                ProjectCheckState::Fail
            },
            check: "git repo",
            detail: if root.join(".git").exists() {
                "detected".into()
            } else {
                "missing .git".into()
            },
        });
    }

    print_project_rows(&rows);
    if health {
        let pass = rows
            .iter()
            .filter(|row| row.state == ProjectCheckState::Pass)
            .count();
        let warn = rows
            .iter()
            .filter(|row| row.state == ProjectCheckState::Warn)
            .count();
        let fail = rows
            .iter()
            .filter(|row| row.state == ProjectCheckState::Fail)
            .count();
        println!("summary: PASS={pass} WARN={warn} FAIL={fail}");
    }
    Ok(())
}

fn sections_check_row(
    inspection: &rtrt_templates::project::ProjectInspection,
    health: bool,
) -> ProjectCheckRow {
    let missing = inspection
        .sections
        .iter()
        .filter(|section| !section.present)
        .map(|section| section.number.to_string())
        .collect::<Vec<_>>();
    let present = inspection.sections.len().saturating_sub(missing.len());
    let state = if missing.is_empty() {
        ProjectCheckState::Pass
    } else if health {
        ProjectCheckState::Fail
    } else {
        ProjectCheckState::Warn
    };
    ProjectCheckRow {
        state,
        check: "sections",
        detail: if missing.is_empty() {
            format!("present {present}/{}", inspection.sections.len())
        } else {
            format!(
                "present {present}/{}; missing {}",
                inspection.sections.len(),
                missing.join(",")
            )
        },
    }
}

fn agents_check_row(
    inspection: &rtrt_templates::project::ProjectInspection,
    health: bool,
) -> ProjectCheckRow {
    let missing = inspection
        .managed_agents
        .iter()
        .filter(|agent| !agent.present)
        .map(|agent| agent.name.clone())
        .collect::<Vec<_>>();
    let present = inspection
        .managed_agents
        .len()
        .saturating_sub(missing.len());
    let state = if missing.is_empty() {
        ProjectCheckState::Pass
    } else if health {
        ProjectCheckState::Fail
    } else {
        ProjectCheckState::Warn
    };
    ProjectCheckRow {
        state,
        check: "managed agents",
        detail: if missing.is_empty() {
            format!("present {present}/{}", inspection.managed_agents.len())
        } else {
            format!(
                "present {present}/{}; missing {}",
                inspection.managed_agents.len(),
                missing.join(",")
            )
        },
    }
}

fn stale_sections_check_row(
    inspection: &rtrt_templates::project::ProjectInspection,
) -> ProjectCheckRow {
    let stale = inspection
        .sections
        .iter()
        .filter(|section| section.stale)
        .map(|section| section.number.to_string())
        .collect::<Vec<_>>();
    ProjectCheckRow {
        state: if stale.is_empty() {
            ProjectCheckState::Pass
        } else {
            ProjectCheckState::Warn
        },
        check: "stale sections",
        detail: if stale.is_empty() {
            "none".into()
        } else {
            stale.join(",")
        },
    }
}

fn extra_sections_check_row(
    inspection: &rtrt_templates::project::ProjectInspection,
) -> ProjectCheckRow {
    let extra = inspection
        .extra_sections
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>();
    ProjectCheckRow {
        state: if extra.is_empty() {
            ProjectCheckState::Pass
        } else {
            ProjectCheckState::Warn
        },
        check: "extra sections",
        detail: if extra.is_empty() {
            "none".into()
        } else {
            extra.join(",")
        },
    }
}

fn duplicate_sections_check_row(
    inspection: &rtrt_templates::project::ProjectInspection,
) -> ProjectCheckRow {
    let duplicate = inspection
        .duplicate_sections
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>();
    ProjectCheckRow {
        state: if duplicate.is_empty() {
            ProjectCheckState::Pass
        } else {
            ProjectCheckState::Fail
        },
        check: "duplicate sections",
        detail: if duplicate.is_empty() {
            "none".into()
        } else {
            duplicate.join(",")
        },
    }
}

fn print_project_rows(rows: &[ProjectCheckRow]) {
    println!(
        "{:<state_width$}  {:<check_width$}  detail",
        "state",
        "check",
        state_width = PROJECT_STATE_WIDTH,
        check_width = PROJECT_CHECK_WIDTH
    );
    for row in rows {
        println!(
            "{:<state_width$}  {:<check_width$}  {}",
            row.state.as_str(),
            row.check,
            row.detail,
            state_width = PROJECT_STATE_WIDTH,
            check_width = PROJECT_CHECK_WIDTH
        );
    }
}

fn run_project_repair(root: &Path, dry_run: bool) -> Result<()> {
    let plan = rtrt_templates::project::plan_repair(root)?;
    if dry_run {
        println!("[dry-run] root: {}", plan.root.display());
        if plan.actions.is_empty() {
            println!("[dry-run] no managed repair actions");
            return Ok(());
        }
        for action in &plan.actions {
            print_repair_action(action, true);
        }
        return Ok(());
    }
    rtrt_templates::project::apply_repair(&plan)?;
    if plan.actions.is_empty() {
        println!("project repair: no managed repair actions");
        return Ok(());
    }
    for action in &plan.actions {
        print_repair_action(action, false);
    }
    Ok(())
}

fn print_repair_action(action: &rtrt_templates::project::RepairAction, dry_run: bool) {
    let prefix = if dry_run {
        "[dry-run] would"
    } else {
        "repaired:"
    };
    match action {
        rtrt_templates::project::RepairAction::CreateContract { path } => {
            println!("{prefix} create {}", path.display());
        }
        rtrt_templates::project::RepairAction::AppendSection { number, title } => {
            println!("{prefix} append ## {number}. {title}");
        }
        rtrt_templates::project::RepairAction::InstallAgent { path } => {
            println!("{prefix} install {}", path.display());
        }
    }
}

fn print_legacy_retirement_action(
    action: &rtrt_templates::project::LegacyOrchestrationRetirementAction,
    dry_run: bool,
) {
    let prefix = if dry_run {
        "[dry-run] would retire legacy orchestration"
    } else {
        "retired legacy orchestration"
    };
    match action {
        rtrt_templates::project::LegacyOrchestrationRetirementAction::RemoveContractSection {
            path,
            backup,
        } => println!(
            "{prefix}: remove owned section from {} (backup {})",
            path.display(),
            backup.display()
        ),
        rtrt_templates::project::LegacyOrchestrationRetirementAction::RemoveAgent {
            path,
            backup,
        } => println!(
            "{prefix}: remove owned agent {} (backup {})",
            path.display(),
            backup.display()
        ),
    }
}

// `ClaudeSettingsStatus` / `claude_settings_status` / `memory_reachable_status`
// live in `setup.rs` now — shared with `rtrt doctor` so the Claude Code
// integration and memory-store probes aren't duplicated across the two
// commands. See the `use setup::{...}` import above.

fn detect_init_vars(target: &Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    let fingerprint = detect_manifest_fingerprint(target);
    let project_name = fingerprint
        .project_name
        .unwrap_or_else(|| rtrt_core::project_for_cwd(target));

    vars.insert("project_name".into(), project_name);
    if let Some(language) = fingerprint.language {
        vars.insert("language".into(), language);
    }
    if let Some(framework) = fingerprint.framework {
        vars.insert("framework".into(), framework);
    }
    vars
}

#[derive(Default)]
struct ManifestFingerprint {
    project_name: Option<String>,
    language: Option<String>,
    framework: Option<String>,
}

fn detect_manifest_fingerprint(root: &Path) -> ManifestFingerprint {
    if root.join("Cargo.toml").exists() {
        return ManifestFingerprint {
            project_name: cargo_package_name(&root.join("Cargo.toml")),
            language: Some("Rust".into()),
            framework: Some("cargo".into()),
        };
    }

    if root.join("package.json").exists() {
        return ManifestFingerprint {
            project_name: package_json_name(&root.join("package.json")),
            language: Some("Node/TypeScript".into()),
            framework: Some(node_package_manager(root).into()),
        };
    }

    if root.join("pyproject.toml").exists() || root.join("requirements.txt").exists() {
        return ManifestFingerprint {
            project_name: python_project_name(root),
            language: Some("Python".into()),
            framework: Some(python_package_manager(root).into()),
        };
    }

    if root.join("go.mod").exists() {
        return ManifestFingerprint {
            project_name: go_module_name(&root.join("go.mod")),
            language: Some("Go".into()),
            framework: Some("go".into()),
        };
    }

    if root.join("pom.xml").exists() {
        return ManifestFingerprint {
            project_name: pom_artifact_name(&root.join("pom.xml")),
            language: Some("Java".into()),
            framework: Some("maven".into()),
        };
    }

    if root.join("build.gradle").exists() || root.join("build.gradle.kts").exists() {
        return ManifestFingerprint {
            project_name: gradle_project_name(root),
            language: Some("Java".into()),
            framework: Some("gradle".into()),
        };
    }

    ManifestFingerprint::default()
}

fn cargo_package_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_package = false;
    for raw in content.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_package = line == "[package]";
            continue;
        }
        if in_package {
            if let Some(value) = line.strip_prefix("name").and_then(toml_value_after_eq) {
                return parse_toml_string(value);
            }
        }
    }
    None
}

fn package_json_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(last_package_segment)
        .filter(|name| !name.is_empty())
}

fn python_project_name(root: &Path) -> Option<String> {
    let path = root.join("pyproject.toml");
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_project = false;
    for raw in content.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_project = line == "[project]" || line == "[tool.poetry]";
            continue;
        }
        if in_project {
            if let Some(value) = line.strip_prefix("name").and_then(toml_value_after_eq) {
                return parse_toml_string(value);
            }
        }
    }
    None
}

fn go_module_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().find_map(|line| {
        let module = line.trim().strip_prefix("module")?.trim();
        if module.is_empty() {
            None
        } else {
            Some(last_package_segment(module))
        }
    })
}

fn pom_artifact_name(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let start = content.find("<artifactId>")? + "<artifactId>".len();
    let rest = &content[start..];
    let end = rest.find("</artifactId>")?;
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn gradle_project_name(root: &Path) -> Option<String> {
    let content = std::fs::read_to_string(root.join("settings.gradle"))
        .or_else(|_| std::fs::read_to_string(root.join("settings.gradle.kts")))
        .ok()?;
    for raw in content.lines() {
        let line = raw.trim();
        if let Some(value) = line.strip_prefix("rootProject.name").and_then(|rest| {
            rest.split_once('=')
                .map(|(_, value)| value.trim().trim_matches('"').trim_matches('\''))
        }) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn toml_value_after_eq(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    rest.strip_prefix('=').map(str::trim)
}

fn last_package_segment(raw: &str) -> String {
    raw.rsplit(['/', ':'])
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

fn node_package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if root.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

fn python_package_manager(root: &Path) -> &'static str {
    if root.join("uv.lock").exists() {
        "uv"
    } else {
        "pip"
    }
}

fn validate_init_template_paths(template: &rtrt_templates::Template) -> Result<()> {
    for file in &template.files {
        validate_relative_template_path(&file.path)?;
    }
    Ok(())
}

fn validate_relative_template_path(path: &str) -> Result<()> {
    if path.starts_with('/') || path.contains("..") {
        bail!("unsafe template file path: {path}");
    }

    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => bail!("unsafe template file path: {path}"),
        }
    }
    Ok(())
}

fn safe_rendered_relative_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let rel = path.strip_prefix(root).with_context(|| {
        format!(
            "rendered template path escapes target directory: {}",
            path.display()
        )
    })?;
    validate_relative_template_path(&rel.to_string_lossy())?;
    Ok(rel.to_path_buf())
}

fn set_executable_if_requested(path: &Path, executable: bool) -> Result<()> {
    if !executable {
        return Ok(());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(path)
            .with_context(|| format!("read permissions: {}", path.display()))?
            .permissions();
        perm.set_mode(perm.mode() | UNIX_EXECUTE_BITS);
        std::fs::set_permissions(path, perm)
            .with_context(|| format!("set executable bit: {}", path.display()))?;
    }

    // The executable bit is a no-op on non-unix targets; `path` is only read
    // inside the `cfg(unix)` block above.
    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

fn run_proxy_run(command: Vec<String>, raw: bool, errors_only: bool, ultra_compact: bool) -> ! {
    let Some(command_text) = shell_command_text(&command) else {
        eprintln!("rtrt proxy-run: command is empty");
        std::process::exit(EXEC_FAILURE_EXIT_CODE);
    };
    let started = std::time::Instant::now();
    let shell_text = command_text_for_capture(&command_text);
    let output = shell_output(&shell_text);
    let output = match output {
        Ok(out) => out,
        Err(err) => {
            eprintln!("rtrt proxy-run: {err}");
            std::process::exit(EXEC_FAILURE_EXIT_CODE);
        }
    };
    let raw_output = String::from_utf8_lossy(&output.stdout).into_owned();
    let input_chars = raw_output.len();
    let mut mode = "passthrough";
    let filtered = if raw {
        mode = "raw";
        raw_output
    } else {
        // Prefer the most specific match on the full command (e.g. `ls -la`
        // selects the long-format filter); fall back to the first token only
        // when the full command has no registered filter.
        let filter = rtrt_proxy::filter_for(&command_text)
            .or_else(|| first_whitespace_token(&command_text).and_then(rtrt_proxy::filter_for));
        if let Some(filter) = filter {
            mode = filter.command;
            filter.apply(&raw_output)
        } else if errors_only {
            mode = "errors-only";
            rtrt_proxy::errors_only(&raw_output, PROXY_RUN_ERROR_CONTEXT_LINES)
        } else if ultra_compact {
            mode = "ultra-compact";
            rtrt_proxy::ultra_compact(&raw_output)
        } else {
            raw_output
        }
    };
    proxy_stats::record_best_effort(proxy_stats_record(
        &command_text,
        mode,
        input_chars,
        filtered.len(),
        started.elapsed(),
    ));
    if let Err(err) = std::io::stdout().write_all(filtered.as_bytes()) {
        eprintln!("rtrt proxy-run: write stdout: {err}");
        std::process::exit(EXEC_FAILURE_EXIT_CODE);
    }
    std::process::exit(output.status.code().unwrap_or(EXEC_FAILURE_EXIT_CODE));
}

fn shell_command_text(command: &[String]) -> Option<String> {
    match command {
        [] => None,
        [single] => {
            let trimmed = single.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        parts => Some(
            parts
                .iter()
                .map(|part| shell_arg(part))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

#[cfg(windows)]
fn shell_arg(arg: &str) -> String {
    if arg.is_empty()
        || arg
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '&' | '|' | '<' | '>' | '^'))
    {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

#[cfg(not(windows))]
fn shell_arg(arg: &str) -> String {
    if arg.is_empty()
        || arg.chars().any(|c| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\'' | '"'
                        | '$'
                        | '`'
                        | '\\'
                        | '|'
                        | '&'
                        | ';'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '!'
                        | '#'
                )
        })
    {
        format!("'{}'", arg.replace('\'', "'\\''"))
    } else {
        arg.to_string()
    }
}

fn command_text_for_capture(command_text: &str) -> String {
    let mut shell_text =
        String::with_capacity(command_text.len() + STDERR_TO_STDOUT_REDIRECT.len());
    shell_text.push_str(command_text);
    shell_text.push_str(STDERR_TO_STDOUT_REDIRECT);
    shell_text
}

fn shell_output(command_text: &str) -> std::io::Result<std::process::Output> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(command_text)
            .output()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(command_text)
            .output()
    }
}

fn first_whitespace_token(input: &str) -> Option<&str> {
    input.split_whitespace().next()
}

fn proxy_stats_record(
    command: &str,
    mode: &str,
    input_chars: usize,
    output_chars: usize,
    elapsed: std::time::Duration,
) -> proxy_stats::ProxyRunRecord {
    let input = input_chars as u64;
    let output = output_chars as u64;
    let saved = input.saturating_sub(output);
    let saved_pct = if input == 0 {
        0.0
    } else {
        (saved as f64 / input as f64) * 100.0
    };
    proxy_stats::ProxyRunRecord {
        project: current_project_name(),
        original_cmd: command.to_string(),
        mode: mode.to_string(),
        input_chars: input,
        output_chars: output,
        saved_chars: saved,
        saved_pct,
        exec_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
    }
}

fn current_project_name() -> String {
    std::env::current_dir()
        .ok()
        .map(|cwd| rtrt_core::project_for_cwd(&cwd))
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::env::current_dir().ok().and_then(|cwd| {
                cwd.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| "default".to_string())
}

/// Printed when `rtrt` runs with no subcommand — a short quickstart instead
/// of clap's raw `--help` dump. `rtrt --help` still shows the full grouped
/// command overview.
fn print_quickstart() {
    let dashboard = effective_config_for_cwd().dashboard.bind;
    println!("rtrt (Retort) — distills AI agent context: memory, compression, proxy, routing.");
    println!();
    println!("  1. rtrt setup --agent claude --apply --plugin   wire hooks + MCP + statusline");
    println!("  2. rtrt doctor                                  confirm the install is healthy");
    println!("  3. rtrt gain                                    see Command Optimizer savings");
    println!("  Dashboard: http://{dashboard} (rtrt service install --apply for always-on)");
    println!();
    println!("Run `rtrt --help` for the full command list.");
}

/// Stack for the worker thread that runs the whole CLI.
///
/// Windows' default main-thread stack is 1 MiB (set by the linker), versus
/// 8 MiB on Linux/macOS. clap's derive-generated `Command` builder for rtrt's
/// ~40 subcommands compiles to one very large function whose (unoptimized,
/// debug-build) frame — evaluated inside `Cli::parse()` before any subcommand
/// dispatch — overflows a 1 MiB stack. On Windows that aborts *every*
/// invocation, including `rtrt --version`, with STATUS_STACK_OVERFLOW
/// (0xC00000FD); the roomier stacks on Linux/macOS simply hid it. Running the
/// CLI on a thread whose stack matches the Unix default makes startup behave
/// identically on every platform.
const MAIN_STACK_SIZE: usize = 8 * 1024 * 1024;

const OPENCODE_NESTED_MARKERS: &[&str] = &[
    "OPENCODE_SESSION_ID",
    "OPENCODE_PROJECT_ID",
    "OPENCODE_SERVER",
    "OPENCODE_PID",
    "RTRT_OPENCODE_PLUGIN_ACTIVE",
    "RTRT_STRICT_CLAUDE_SANDBOX",
];

#[derive(Debug)]
struct OpenCodeProjectPaths {
    data: PathBuf,
    state: PathBuf,
    db: PathBuf,
}

fn refuse_nested_opencode() -> Result<()> {
    if let Some(marker) = nested_opencode_marker(|marker| std::env::var_os(marker).is_some()) {
        bail!("refusing nested OpenCode launch: {marker} is set");
    }
    Ok(())
}

fn nested_opencode_marker(mut present: impl FnMut(&str) -> bool) -> Option<&'static str> {
    OPENCODE_NESTED_MARKERS
        .iter()
        .copied()
        .find(|marker| present(marker))
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "private OpenCode path is not a real directory: {}",
                    path.display()
                );
            }
            let uid = unsafe_geteuid();
            if metadata.uid() != uid {
                bail!(
                    "private OpenCode directory has unsafe owner: {}",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("private OpenCode directory has no parent"))?;
            if !parent.exists() {
                ensure_private_directory(parent)?;
            }
            std::fs::create_dir(path)
                .with_context(|| format!("create private directory {}", path.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn unsafe_geteuid() -> u32 {
    // Read from proc instead of introducing libc or an unsafe FFI block.
    std::fs::metadata("/proc/self").map_or_else(
        |_| {
            std::fs::metadata(".").map_or(0, |metadata| {
                use std::os::unix::fs::MetadataExt;
                metadata.uid()
            })
        },
        |metadata| {
            use std::os::unix::fs::MetadataExt;
            metadata.uid()
        },
    )
}

#[cfg(not(unix))]
fn ensure_private_directory(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            std::fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "private OpenCode path is not a real directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn ensure_private_file(path: &Path) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "private OpenCode file is not a real regular file: {}",
                    path.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if metadata.uid() != unsafe_geteuid()
                    || metadata.permissions().mode() & 0o777 != 0o600
                {
                    bail!(
                        "private OpenCode file has unsafe owner or mode: {}",
                        path.display()
                    );
                }
            }
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

fn global_xdg_data_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
}

fn copy_opencode_auth_once(global_data: &Path, private_data: &Path) -> Result<()> {
    let source = global_data.join("opencode/auth.json");
    let destination = private_data.join("opencode/auth.json");
    if std::fs::symlink_metadata(&destination).is_ok() {
        let metadata = std::fs::symlink_metadata(&destination)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "refusing unsafe private OpenCode auth destination: {}",
                destination.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != unsafe_geteuid() || metadata.permissions().mode() & 0o777 != 0o600
            {
                bail!(
                    "private OpenCode auth destination has unsafe owner or mode: {}",
                    destination.display()
                );
            }
        }
        return Ok(());
    }
    if !source.exists() {
        return Ok(());
    }
    let mut source_file = std::fs::File::open(&source)?;
    let metadata = source_file.metadata()?;
    let path_metadata = std::fs::symlink_metadata(&source)?;
    if path_metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing unsafe global OpenCode auth source: {}",
            source.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.dev() != path_metadata.dev()
            || metadata.ino() != path_metadata.ino()
            || metadata.uid() != unsafe_geteuid()
            || metadata.permissions().mode() & 0o077 != 0
        {
            bail!(
                "global OpenCode auth source has unsafe owner or mode: {}",
                source.display()
            );
        }
    }
    if metadata.len() > 1024 * 1024 {
        bail!("global OpenCode auth source exceeds 1 MiB");
    }
    ensure_private_directory(
        destination
            .parent()
            .context("auth destination has no parent")?,
    )?;
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut source_file)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut contents)?;
    if contents.len() > 1024 * 1024 {
        bail!("global OpenCode auth source exceeds 1 MiB");
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(&destination)?.write_all(&contents)?;
    Ok(())
}

fn prepare_opencode_project(
    identity: &ProjectIdentity,
    home: &Path,
) -> Result<OpenCodeProjectPaths> {
    let rtrt = home.join(".rtrt");
    let projects = rtrt.join("projects");
    let project = rtrt_core::project::project_storage_dir_in(home, identity);
    let root = project.join("opencode");
    let data = root.join("data");
    let state = root.join("state");
    let app_data = data.join("opencode");
    for directory in [&rtrt, &projects, &project, &root, &data, &state] {
        ensure_private_directory(directory)?;
    }
    ensure_private_directory(&app_data)?;
    ensure_private_directory(&state.join("opencode"))?;
    let db = app_data.join("opencode.db");
    ensure_private_file(&db)?;
    copy_opencode_auth_once(&global_xdg_data_home(home), &data)?;
    Ok(OpenCodeProjectPaths { data, state, db })
}

fn trusted_direct_launch_candidate(
    boundary: &sandbox::ProjectBoundary,
    candidate: &Path,
) -> Result<PathBuf> {
    sandbox::validate_direct_launch_executable(boundary, candidate)?;
    let canonical = std::fs::canonicalize(candidate)
        .with_context(|| format!("canonicalize OpenCode executable {}", candidate.display()))?;
    if canonical != candidate {
        bail!(
            "OpenCode executable must already be a canonical path without symlinks: {}",
            candidate.display()
        );
    }
    sandbox::validate_direct_launch_executable(boundary, &canonical)?;
    Ok(canonical)
}

fn first_trusted_opencode_candidate(
    boundary: &sandbox::ProjectBoundary,
    candidates: impl IntoIterator<Item = PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> Option<PathBuf> {
    candidates.into_iter().find_map(|candidate| {
        let canonical = trusted_direct_launch_candidate(boundary, &candidate).ok()?;
        seen.insert(canonical.clone()).then_some(canonical)
    })
}

fn opencode_path_candidates(path: Option<OsString>) -> Vec<PathBuf> {
    path.into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|component| !component.as_os_str().is_empty() && component.is_absolute())
        .map(|component| component.join("opencode"))
        .collect()
}

fn resolve_trusted_opencode(identity: &ProjectIdentity) -> Result<PathBuf> {
    let home = setup::dirs_home()?;
    let candidates = [
        home.join(".opencode/bin/opencode"),
        home.join(".local/bin/opencode"),
        home.join(".cargo/bin/opencode"),
        PathBuf::from("/usr/local/bin/opencode"),
        PathBuf::from("/usr/bin/opencode"),
        PathBuf::from("/opt/homebrew/bin/opencode"),
    ];
    let boundary = sandbox::ProjectBoundary {
        root: identity.checkout_root().to_path_buf(),
        cwd: identity.checkout_root().to_path_buf(),
        git_writable: Vec::new(),
    };
    if let Some(configured) = std::env::var_os("RTRT_OPENCODE_BIN") {
        let configured = PathBuf::from(configured);
        return trusted_direct_launch_candidate(&boundary, &configured).with_context(|| {
            format!(
                "RTRT_OPENCODE_BIN is not a trusted executable: {}",
                configured.display()
            )
        });
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(executable) = first_trusted_opencode_candidate(&boundary, candidates, &mut seen) {
        return Ok(executable);
    }
    let path_candidates = opencode_path_candidates(std::env::var_os("PATH"));
    if let Some(executable) =
        first_trusted_opencode_candidate(&boundary, path_candidates, &mut seen)
    {
        return Ok(executable);
    }
    bail!("trusted OpenCode executable not found in fixed locations or absolute PATH components")
}

#[cfg(all(test, unix))]
mod trusted_opencode_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn executable(path: &Path, marker: &Path) {
        std::fs::write(path, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn fixture() -> (
        tempfile::TempDir,
        sandbox::ProjectBoundary,
        PathBuf,
        PathBuf,
    ) {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let base = tempfile::Builder::new()
            .prefix("trusted-opencode-")
            .tempdir_in(workspace)
            .unwrap();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let project = base.path().join("project");
        let bin = base.path().join("safe-bin");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let marker = base.path().join("executed");
        let boundary = sandbox::ProjectBoundary {
            root: project.clone(),
            cwd: project,
            git_writable: Vec::new(),
        };
        (base, boundary, bin, marker)
    }

    #[test]
    fn safe_absolute_path_candidate_is_accepted_without_execution() {
        let (_base, boundary, bin, marker) = fixture();
        let candidate = bin.join("opencode");
        executable(&candidate, &marker);
        let path = std::env::join_paths([bin]).unwrap();
        let candidates = opencode_path_candidates(Some(path));
        let resolved = first_trusted_opencode_candidate(
            &boundary,
            candidates,
            &mut std::collections::HashSet::new(),
        );
        assert_eq!(resolved.as_deref(), Some(candidate.as_path()));
        assert!(!marker.exists(), "candidate discovery executed OpenCode");
    }

    #[test]
    fn unsafe_path_candidates_are_rejected() {
        let (_base, boundary, bin, marker) = fixture();
        let safe = bin.join("opencode");
        executable(&safe, &marker);

        let project_bin = boundary.root.join("bin");
        std::fs::create_dir(&project_bin).unwrap();
        let project_candidate = project_bin.join("opencode");
        executable(&project_candidate, &marker);
        assert!(trusted_direct_launch_candidate(&boundary, &project_candidate).is_err());

        let writable_bin = bin.parent().unwrap().join("writable-bin");
        std::fs::create_dir(&writable_bin).unwrap();
        std::fs::set_permissions(&writable_bin, std::fs::Permissions::from_mode(0o777)).unwrap();
        let writable_candidate = writable_bin.join("opencode");
        executable(&writable_candidate, &marker);
        assert!(trusted_direct_launch_candidate(&boundary, &writable_candidate).is_err());

        let link = bin.join("linked-opencode");
        symlink(&safe, &link).unwrap();
        assert!(trusted_direct_launch_candidate(&boundary, &link).is_err());

        let relative = std::env::join_paths([PathBuf::from("relative"), PathBuf::new()]).unwrap();
        assert!(opencode_path_candidates(Some(relative)).is_empty());
        assert!(!marker.exists(), "candidate validation executed OpenCode");
    }

    #[test]
    fn installer_config_evidence_does_not_require_fixed_opencode_path() {
        let installer = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../install.sh"));
        assert!(installer.contains("if [ \"$evidence\" -eq 0 ]; then\n        opencode_evidence=\"$(command -v opencode 2>/dev/null || true)\""));
        assert!(!installer.contains("for candidate in /usr/bin/opencode"));
    }

    #[test]
    fn continue_detection_accepts_only_exact_boolean_forms() {
        assert!(requests_opencode_continue(&[OsString::from("-c")]));
        assert!(requests_opencode_continue(&[OsString::from("--continue")]));
        assert!(!requests_opencode_continue(&[OsString::from(
            "--continue=true"
        )]));
        assert!(!requests_opencode_continue(&[OsString::from("topic-c")]));
    }

    #[test]
    fn runtime_probe_uses_fixed_argv_private_env_and_checkout_cwd() {
        let (base, _boundary, bin, _marker) = fixture();
        let checkout = base.path().join("project");
        std::fs::create_dir(checkout.join(".git")).unwrap();
        let identity = ProjectIdentity::derive(&checkout).unwrap();
        let log = base.path().join("probe.log");
        let executable = bin.join("opencode");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" \"$XDG_DATA_HOME\" \"$XDG_STATE_HOME\" \"$OPENCODE_DB\" \"$PWD\" > '{}'\nprintf forbidden-output\nprintf forbidden-error >&2\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = OpenCodeProjectPaths {
            data: base.path().join("private/data"),
            state: base.path().join("private/state"),
            db: base.path().join("private/data/opencode/opencode.db"),
        };
        assert!(
            probe_opencode_runtime_project(&executable, &identity, &paths)
                .unwrap()
                .success()
        );
        let lines = std::fs::read_to_string(log).unwrap();
        let expected = format!(
            "session list --max-count 1 --format json\n{}\n{}\n{}\n{}\n",
            paths.data.display(),
            paths.state.display(),
            paths.db.display(),
            identity.checkout_root().display()
        );
        assert_eq!(lines, expected);
    }

    #[test]
    fn opencode_project_cache_is_strict_and_common_to_linked_worktrees() {
        let (base, mut boundary, _bin, _marker) = fixture();
        let common = base.path().join("common.git");
        std::fs::create_dir(&common).unwrap();
        boundary.git_writable = vec![common.clone(), base.path().join("linked-admin")];
        assert_eq!(opencode_common_git_dir(&boundary), common);
        std::fs::write(
            common.join("opencode"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(
            opencode_sessions::read_runtime_project_cache(&common).unwrap(),
            Some("0123456789abcdef0123456789abcdef01234567".to_string())
        );

        boundary.git_writable.clear();
        assert_eq!(
            opencode_common_git_dir(&boundary),
            boundary.root.join(".git")
        );
        std::fs::write(common.join("opencode"), "stale").unwrap();
        assert!(opencode_sessions::read_runtime_project_cache(&common).is_err());
    }

    #[test]
    fn post_probe_authority_fails_closed_without_cache_on_failure() {
        let runtime = "0123456789abcdef0123456789abcdef01234567".to_string();
        assert_eq!(runtime_authority_after_probe(true, None).unwrap(), "global");
        assert_eq!(
            runtime_authority_after_probe(true, Some(runtime.clone())).unwrap(),
            runtime
        );
        assert_eq!(
            runtime_authority_after_probe(false, Some(runtime.clone())).unwrap(),
            runtime
        );
        assert!(runtime_authority_after_probe(false, None).is_err());
        assert!(should_refresh_runtime_checkpoint(true, true));
        assert!(!should_refresh_runtime_checkpoint(false, true));
        assert!(!should_refresh_runtime_checkpoint(true, false));
    }
}

fn validate_opencode_args(args: &[OsString], identity: &ProjectIdentity) -> Result<()> {
    let mut directory_value = false;
    for arg in args {
        let path = PathBuf::from(arg);
        if directory_value {
            let selected = ProjectIdentity::derive(&path).with_context(|| {
                format!("resolve OpenCode directory argument {}", path.display())
            })?;
            if selected.fingerprint() != identity.fingerprint() {
                bail!(
                    "OpenCode directory argument selects a different project: {}",
                    path.display()
                );
            }
            directory_value = false;
            continue;
        }
        let text = arg.to_string_lossy();
        if matches!(text.as_ref(), "--directory" | "--dir" | "--cwd") {
            directory_value = true;
            continue;
        }
        if let Some(value) = ["--directory=", "--dir=", "--cwd="]
            .iter()
            .find_map(|prefix| text.strip_prefix(prefix))
        {
            let selected = ProjectIdentity::derive(value)?;
            if selected.fingerprint() != identity.fingerprint() {
                bail!("OpenCode directory argument selects a different project: {value}");
            }
        } else if path.is_dir() {
            let selected = ProjectIdentity::derive(&path)?;
            if selected.fingerprint() != identity.fingerprint() {
                bail!(
                    "OpenCode directory argument selects a different project: {}",
                    path.display()
                );
            }
        }
    }
    if directory_value {
        bail!("OpenCode directory option is missing its value");
    }
    Ok(())
}

fn opencode_process(
    executable: &Path,
    identity: &ProjectIdentity,
    paths: &OpenCodeProjectPaths,
    args: &[OsString],
) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command
        .args(args)
        .current_dir(identity.checkout_root())
        .env("XDG_DATA_HOME", &paths.data)
        .env("XDG_STATE_HOME", &paths.state)
        .env("OPENCODE_DB", &paths.db);
    command
}

fn requests_opencode_continue(args: &[OsString]) -> bool {
    args.iter().any(|arg| arg == "-c" || arg == "--continue")
}

fn opencode_common_git_dir(boundary: &sandbox::ProjectBoundary) -> PathBuf {
    boundary
        .git_writable
        .first()
        .cloned()
        .unwrap_or_else(|| boundary.root.join(".git"))
}

fn probe_opencode_runtime_project(
    executable: &Path,
    identity: &ProjectIdentity,
    paths: &OpenCodeProjectPaths,
) -> Result<std::process::ExitStatus> {
    opencode_process(
        executable,
        identity,
        paths,
        &[
            OsString::from("session"),
            OsString::from("list"),
            OsString::from("--max-count"),
            OsString::from("1"),
            OsString::from("--format"),
            OsString::from("json"),
        ],
    )
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .with_context(|| {
        format!(
            "probe OpenCode runtime project using {}",
            executable.display()
        )
    })
}

fn report_nonfatal_opencode_catch_up(result: Result<opencode_sessions::MigrationReport>) -> bool {
    match result {
        Ok(report) => {
            if report.private_preserved_conflicts > 0 || report.archived_event_forks > 0 {
                eprintln!(
                    "OpenCode session catch-up preserved {} private conflict(s); archived {} event fork(s)",
                    report.private_preserved_conflicts, report.archived_event_forks
                );
            } else if report.skipped_locked {
                eprintln!("OpenCode session catch-up deferred: another migration is active");
            }
            true
        }
        Err(_) => {
            eprintln!(
                "warning: OpenCode session catch-up skipped; private launch will continue; run `rtrt opencode sessions status` then `rtrt opencode sessions apply` from a trusted terminal"
            );
            false
        }
    }
}

fn run_opencode_launcher(project: Option<PathBuf>, args: Vec<OsString>) -> Result<()> {
    refuse_nested_opencode()?;
    let selected = project.unwrap_or(std::env::current_dir()?);
    let boundary = sandbox::discover_project_from(&selected)?;
    let identity = ProjectIdentity::derive(&selected)?;
    if identity.checkout_root() != boundary.root {
        bail!("selected checkout identity and strict sandbox boundary disagree");
    }
    validate_opencode_args(&args, &identity)?;
    let authorized = sandbox::authorize_project_for_launch(&boundary).with_context(|| {
        "strict OpenCode sandbox is not ready; run `rtrt setup --agent opencode --sandbox --apply` once from a trusted checkout"
    })?;
    if authorized {
        println!(
            "authorized strict OpenCode sandbox for {}",
            boundary.root.display()
        );
    }
    let home = setup::dirs_home()?;
    let paths = prepare_opencode_project(&identity, &home)?;
    // Incremental, compare-before-insert catch-up handles sessions created by
    // direct global OpenCode after installation and never replaces private rows.
    report_nonfatal_opencode_catch_up(opencode_sessions::migrate(
        &opencode_sessions::MigrationOptions {
            home: home.clone(),
            source: None,
            mode: opencode_sessions::MigrationMode::CatchUp,
        },
    ));
    let executable = resolve_trusted_opencode(&identity)?;
    let common_git = opencode_common_git_dir(&boundary);
    let cached_runtime_id = opencode_sessions::read_runtime_project_cache(&common_git)?;
    let (repair, runtime_id) = if let Some(runtime_id) = cached_runtime_id {
        match opencode_sessions::repair_runtime_attribution(&paths.db, &runtime_id, false)? {
            opencode_sessions::RuntimeRepair::Complete(report) => (report, runtime_id),
            opencode_sessions::RuntimeRepair::NeedsProbe => {
                probe_then_repair(&executable, &identity, &paths, &common_git)?
            }
        }
    } else {
        match opencode_sessions::repair_runtime_attribution(&paths.db, "global", false)? {
            opencode_sessions::RuntimeRepair::Complete(report) => (report, "global".to_string()),
            opencode_sessions::RuntimeRepair::NeedsProbe => {
                // Stale project_directory rows are not runtime evidence.
                probe_then_repair(&executable, &identity, &paths, &common_git)?
            }
        }
    };
    if repair.quarantined_sessions > 0 {
        eprintln!(
            "OpenCode private-history repair quarantined {} malformed session(s) under {}",
            repair.quarantined_sessions,
            repair
                .archive
                .as_deref()
                .unwrap_or_else(|| Path::new("rtrt-repair-archives"))
                .display()
        );
    }
    if requests_opencode_continue(&args) && repair.valid_root_sessions == 0 {
        bail!(
            "OpenCode --continue requested, but no valid private root session exists; launch without -c to create one"
        );
    }
    let status = opencode_process(&executable, &identity, &paths, &args)
        .status()
        .with_context(|| format!("launch {}", executable.display()))?;
    if status.success() {
        let post_cache = opencode_sessions::read_runtime_project_cache(&common_git)?;
        let post_authority = post_cache.unwrap_or_else(|| "global".to_string());
        if should_refresh_runtime_checkpoint(true, post_authority == runtime_id) {
            if let Err(error) =
                opencode_sessions::refresh_runtime_checkpoint(&paths.db, &runtime_id)
            {
                eprintln!(
                    "warning: OpenCode runtime checkpoint refresh failed; next launch will perform full repair: {error}"
                );
            }
        } else {
            eprintln!(
                "warning: OpenCode runtime authority changed during launch; next launch will perform full repair"
            );
        }
    }
    if !status.success() {
        debug_assert!(!should_refresh_runtime_checkpoint(false, true));
        bail!("OpenCode exited with status {status}");
    }
    Ok(())
}

fn should_refresh_runtime_checkpoint(child_succeeded: bool, authority_unchanged: bool) -> bool {
    child_succeeded && authority_unchanged
}

fn probe_then_repair(
    executable: &Path,
    identity: &ProjectIdentity,
    paths: &OpenCodeProjectPaths,
    common_git: &Path,
) -> Result<(opencode_sessions::RuntimeRepairReport, String)> {
    let probe_status = probe_opencode_runtime_project(executable, identity, paths)?;
    let runtime_id = runtime_authority_after_probe(
        probe_status.success(),
        opencode_sessions::read_runtime_project_cache(common_git)?,
    )?;
    let report = match opencode_sessions::repair_runtime_attribution(&paths.db, &runtime_id, true)?
    {
        opencode_sessions::RuntimeRepair::Complete(report) => report,
        opencode_sessions::RuntimeRepair::NeedsProbe => {
            bail!("OpenCode runtime project attribution remains unresolved after trusted probe")
        }
    };
    // OpenCode 1.18.11 may return nonzero after persisting project evidence
    // when decoding a malformed legacy session. No other failure is ignored.
    if !probe_status.success() && report.quarantined_sessions == 0 {
        bail!("OpenCode runtime project probe exited with status {probe_status}")
    }
    Ok((report, runtime_id))
}

fn runtime_authority_after_probe(
    probe_succeeded: bool,
    cached_runtime_id: Option<String>,
) -> Result<String> {
    match (probe_succeeded, cached_runtime_id) {
        (_, Some(runtime_id)) => Ok(runtime_id),
        (true, None) => Ok("global".to_string()),
        (false, None) => bail!(
            "OpenCode runtime project probe failed without committing project authority; refusing global attribution"
        ),
    }
}

fn global_prompt_history_paths(home: &Path) -> [PathBuf; 2] {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    [
        state.join("opencode/prompt-history.jsonl"),
        global_xdg_data_home(home).join("opencode/prompt-history.jsonl"),
    ]
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryQuarantinePhase {
    AfterOpen,
    BeforeLink,
    AfterLink,
    BeforeUnlink,
}

#[cfg(unix)]
fn same_opened_file(opened: &std::fs::Metadata, path: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    opened.dev() == path.dev() && opened.ino() == path.ino()
}

#[cfg(unix)]
fn remove_created_history_link(destination: &Path, opened: &std::fs::Metadata) {
    let Ok(metadata) = std::fs::symlink_metadata(destination) else {
        return;
    };
    if !metadata.file_type().is_symlink() && same_opened_file(opened, &metadata) {
        let _ = std::fs::remove_file(destination);
    }
}

#[cfg(unix)]
fn quarantine_prompt_history_with(
    source: &Path,
    destination: &Path,
    mut phase: impl FnMut(HistoryQuarantinePhase),
) -> Result<bool> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    anyhow::ensure!(
        source.parent() == destination.parent(),
        "prompt-history quarantine destination must be a sibling"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // Linux exposes O_NOFOLLOW through OpenOptionsExt but std does not publish
    // the flag constant. Other Unix targets still reject a followed symlink by
    // comparing the opened inode with symlink_metadata below before mutation.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0o400000);
    let opened = match options.open(source) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "open prompt history without following symlinks: {}",
                    source.display()
                )
            });
        }
    };
    let opened_metadata = opened.metadata()?;
    if !opened_metadata.is_file() || opened_metadata.uid() != unsafe_geteuid() {
        bail!(
            "prompt history has unsafe type or owner: {}",
            source.display()
        );
    }

    phase(HistoryQuarantinePhase::AfterOpen);
    let path_metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("reinspect prompt history: {}", source.display()))?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !same_opened_file(&opened_metadata, &path_metadata)
    {
        bail!("prompt history changed while opening: {}", source.display());
    }

    phase(HistoryQuarantinePhase::BeforeLink);
    std::fs::hard_link(source, destination).with_context(|| {
        format!(
            "create no-overwrite prompt-history quarantine {}",
            destination.display()
        )
    })?;

    phase(HistoryQuarantinePhase::AfterLink);
    let destination_metadata = match std::fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("verify prompt-history quarantine {}", destination.display())
            });
        }
    };
    if destination_metadata.file_type().is_symlink()
        || !destination_metadata.is_file()
        || !same_opened_file(&opened_metadata, &destination_metadata)
    {
        // Do not remove a destination an attacker exchanged after hard_link.
        bail!(
            "prompt-history quarantine destination changed: {}",
            destination.display()
        );
    }

    phase(HistoryQuarantinePhase::BeforeUnlink);
    let source_metadata = std::fs::symlink_metadata(source).with_context(|| {
        format!(
            "reinspect prompt history before unlink: {}",
            source.display()
        )
    })?;
    if source_metadata.file_type().is_symlink()
        || !source_metadata.is_file()
        || !same_opened_file(&opened_metadata, &source_metadata)
    {
        remove_created_history_link(destination, &opened_metadata);
        bail!("prompt history changed before unlink: {}", source.display());
    }

    if let Err(error) = opened.set_permissions(std::fs::Permissions::from_mode(0o600)) {
        remove_created_history_link(destination, &opened_metadata);
        return Err(error).context("chmod opened prompt history to 0600");
    }
    // remove_file unlinks the directory entry itself and never follows a final
    // symlink. A final identity check narrows replacement races before unlink.
    let source_metadata = std::fs::symlink_metadata(source)?;
    if source_metadata.file_type().is_symlink()
        || !same_opened_file(&opened_metadata, &source_metadata)
    {
        remove_created_history_link(destination, &opened_metadata);
        bail!("prompt history changed before unlink: {}", source.display());
    }
    if let Err(error) = std::fs::remove_file(source) {
        if std::fs::symlink_metadata(source)
            .is_ok_and(|metadata| same_opened_file(&opened_metadata, &metadata))
        {
            remove_created_history_link(destination, &opened_metadata);
        }
        return Err(error).with_context(|| format!("unlink prompt history: {}", source.display()));
    }
    Ok(true)
}

#[cfg(unix)]
fn quarantine_prompt_history(source: &Path, destination: &Path) -> Result<bool> {
    quarantine_prompt_history_with(source, destination, |_| {})
}

#[cfg(not(unix))]
fn quarantine_prompt_history(_source: &Path, _destination: &Path) -> Result<bool> {
    bail!("secure prompt-history quarantine is unsupported on this platform")
}

fn run_opencode_history(apply: Option<bool>) -> Result<()> {
    let home = setup::dirs_home()?;
    for path in global_prompt_history_paths(&home) {
        let destination = path.with_file_name("prompt-history.jsonl.rtrt-quarantine");
        if apply == Some(true) {
            if quarantine_prompt_history(&path, &destination)? {
                println!(
                    "quarantined {} -> {}",
                    path.display(),
                    destination.display()
                );
            } else {
                println!("absent {}", path.display());
            }
            continue;
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                println!("absent {}", path.display());
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "refusing non-regular or symlink prompt history: {}",
                path.display()
            );
        }
        if apply.is_none() {
            println!("present {}", path.display());
            continue;
        }
        if apply == Some(false) {
            println!(
                "[dry-run] would quarantine {} -> {}",
                path.display(),
                destination.display()
            );
            continue;
        }
    }
    Ok(())
}

#[cfg(test)]
mod opencode_launcher_tests {
    use super::*;
    use std::fs;

    fn repo(path: &Path) {
        fs::create_dir_all(path.join(".git")).unwrap();
    }

    fn linked_worktree(main: &Path, linked: &Path) {
        let admin = main.join(".git/worktrees/linked");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(linked).unwrap();
        let dot_git = linked.join(".git");
        fs::write(&dot_git, format!("gitdir: {}\n", admin.display())).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("gitdir"), format!("{}\n", dot_git.display())).unwrap();
    }

    #[test]
    fn future_global_schema_catch_up_error_does_not_block_child_path() {
        let continued = !report_nonfatal_opencode_catch_up(Err(anyhow::anyhow!(
            "unknown OpenCode graph-dependent table: future_session_graph"
        )));
        assert!(continued);
        let child_launch_path_reached = true;
        assert!(child_launch_path_reached);
    }

    #[cfg(unix)]
    fn private_history(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_quarantine_never_overwrites_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prompt-history.jsonl");
        let destination = temp.path().join("prompt-history.jsonl.rtrt-quarantine");
        private_history(&source, "source");
        private_history(&destination, "existing");

        let error = quarantine_prompt_history(&source, &destination).unwrap_err();

        assert!(error.to_string().contains("create no-overwrite"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "existing");
    }

    #[cfg(unix)]
    #[test]
    fn history_quarantine_rejects_source_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let source = temp.path().join("prompt-history.jsonl");
        let destination = temp.path().join("prompt-history.jsonl.rtrt-quarantine");
        private_history(&target, "target");
        symlink(&target, &source).unwrap();

        assert!(quarantine_prompt_history(&source, &destination).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "target");
        assert!(
            fs::symlink_metadata(&source)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn history_quarantine_detects_source_exchange_after_open() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prompt-history.jsonl");
        let original = temp.path().join("original");
        let destination = temp.path().join("prompt-history.jsonl.rtrt-quarantine");
        private_history(&source, "original");

        let error = quarantine_prompt_history_with(&source, &destination, |phase| {
            if phase == HistoryQuarantinePhase::AfterOpen {
                fs::rename(&source, &original).unwrap();
                private_history(&source, "replacement");
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("changed while opening"));
        assert_eq!(fs::read_to_string(&original).unwrap(), "original");
        assert_eq!(fs::read_to_string(&source).unwrap(), "replacement");
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn history_quarantine_does_not_remove_exchanged_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prompt-history.jsonl");
        let destination = temp.path().join("prompt-history.jsonl.rtrt-quarantine");
        let displaced_link = temp.path().join("displaced-quarantine-link");
        private_history(&source, "original");

        let error = quarantine_prompt_history_with(&source, &destination, |phase| {
            if phase == HistoryQuarantinePhase::AfterLink {
                fs::rename(&destination, &displaced_link).unwrap();
                private_history(&destination, "replacement");
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("destination changed"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "original");
        assert_eq!(fs::read_to_string(&destination).unwrap(), "replacement");
        assert_eq!(fs::read_to_string(&displaced_link).unwrap(), "original");
    }

    #[cfg(unix)]
    #[test]
    fn history_quarantine_detects_source_exchange_before_unlink_and_cleans_own_link() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prompt-history.jsonl");
        let original = temp.path().join("original");
        let destination = temp.path().join("prompt-history.jsonl.rtrt-quarantine");
        private_history(&source, "original");

        let error = quarantine_prompt_history_with(&source, &destination, |phase| {
            if phase == HistoryQuarantinePhase::BeforeUnlink {
                fs::rename(&source, &original).unwrap();
                private_history(&source, "replacement");
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("changed before unlink"));
        assert_eq!(fs::read_to_string(&original).unwrap(), "original");
        assert_eq!(fs::read_to_string(&source).unwrap(), "replacement");
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn history_quarantine_links_chmods_handle_then_unlinks_source() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prompt-history.jsonl");
        let destination = temp.path().join("prompt-history.jsonl.rtrt-quarantine");
        private_history(&source, "private");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(quarantine_prompt_history(&source, &destination).unwrap());

        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&destination).unwrap(), "private");
        assert_eq!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn same_basename_projects_get_distinct_opencode_data() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("one/repo");
        let second = temp.path().join("two/repo");
        repo(&first);
        repo(&second);
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();

        let first =
            prepare_opencode_project(&ProjectIdentity::derive(first).unwrap(), &home).unwrap();
        let second =
            prepare_opencode_project(&ProjectIdentity::derive(second).unwrap(), &home).unwrap();
        assert_ne!(first.data, second.data);
        assert_ne!(first.db, second.db);
    }

    #[test]
    fn linked_worktree_shares_data_but_keeps_checkout_cwd_and_argv() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let linked = temp.path().join("linked");
        repo(&main);
        linked_worktree(&main, &linked);
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let main_identity = ProjectIdentity::derive(&main).unwrap();
        let linked_identity = ProjectIdentity::derive(&linked).unwrap();
        let main_paths = prepare_opencode_project(&main_identity, &home).unwrap();
        let linked_paths = prepare_opencode_project(&linked_identity, &home).unwrap();
        assert_eq!(main_paths.data, linked_paths.data);

        let args = [
            OsString::from("--model"),
            OsString::from("provider/model with space"),
        ];
        let command = opencode_process(
            Path::new("/usr/bin/opencode"),
            &linked_identity,
            &linked_paths,
            &args,
        );
        assert_eq!(command.get_current_dir(), Some(linked.as_path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            args.iter().map(OsString::as_os_str).collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_db_rejects_unsafe_mode_and_data_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        repo(&project);
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let identity = ProjectIdentity::derive(&project).unwrap();
        let paths = prepare_opencode_project(&identity, &home).unwrap();
        fs::set_permissions(&paths.db, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(prepare_opencode_project(&identity, &home).is_err());

        fs::set_permissions(&paths.db, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(&paths.data).unwrap();
        symlink(temp.path(), &paths.data).unwrap();
        assert!(prepare_opencode_project(&identity, &home).is_err());
    }

    #[test]
    fn nested_markers_refuse_launch() {
        assert_eq!(
            nested_opencode_marker(|marker| marker == "OPENCODE_SESSION_ID"),
            Some("OPENCODE_SESSION_ID")
        );
        assert_eq!(nested_opencode_marker(|_| false), None);
    }
}

fn main() -> Result<()> {
    if let Some(code) = sandbox::dispatch_from_env()? {
        std::process::exit(code);
    }
    let worker = std::thread::Builder::new()
        .name("rtrt-main".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(run_cli)
        .context("spawn rtrt worker thread")?;
    match worker.join() {
        Ok(result) => result,
        // The worker already printed its panic message; mirror Rust's default
        // panic exit status instead of re-panicking on the small main stack.
        Err(_) => std::process::exit(101),
    }
}

/// Initialise tracing, parse the CLI, and drive the async dispatch on a
/// manually built multi-thread Tokio runtime. Runs on the generous-stack
/// worker thread spawned by `main` (see `MAIN_STACK_SIZE`).
fn run_cli() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info")
        .init();
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        print_quickstart();
        return Ok(());
    };
    let command = match command {
        Cmd::Statusline {
            opencode: true,
            cwd,
            session,
            model,
            width,
            budget_ms,
            no_git,
            refresh,
            ..
        } => {
            print_opencode_statusline(OpenCodeStatuslineOptions {
                cwd,
                session,
                model,
                width,
                budget_ms,
                no_git,
                refresh,
            });
            return Ok(());
        }
        command => command,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(run(command))
}

/// Dispatch a parsed subcommand.
async fn run(command: Cmd) -> Result<()> {
    match command {
        Cmd::Opencode {
            project,
            action,
            args,
        } => match action {
            Some(OpenCodeAction::Sessions { command }) => {
                let mode = match command {
                    OpenCodeSessionsAction::Status => opencode_sessions::MigrationMode::Status,
                    OpenCodeSessionsAction::DryRun => opencode_sessions::MigrationMode::DryRun,
                    OpenCodeSessionsAction::Apply => opencode_sessions::MigrationMode::Apply,
                };
                let report = opencode_sessions::migrate(&opencode_sessions::MigrationOptions {
                    home: setup::dirs_home()?,
                    source: None,
                    mode,
                })?;
                if let Some(source) = report.source {
                    println!(
                        "source={} sessions={} projects={} archived={} skipped_malformed={} rows={} changed={} private_preserved_conflicts={} archived_event_forks={}",
                        source.display(),
                        report.sessions,
                        report.projects,
                        report.archived_sessions,
                        report.skipped_malformed_sessions,
                        report.rows,
                        report.changed_rows,
                        report.private_preserved_conflicts,
                        report.archived_event_forks
                    );
                } else {
                    println!("no supported global OpenCode database found");
                }
            }
            Some(OpenCodeAction::HistoryStatus) => run_opencode_history(None)?,
            Some(OpenCodeAction::HistoryQuarantine { apply }) => run_opencode_history(Some(apply))?,
            None => run_opencode_launcher(project, args)?,
        },
        Cmd::Compress {
            level,
            file,
            in_place,
            backup,
            llm,
            provider,
            model,
            base_url,
            format,
            ml,
            ratio,
            onnx_model,
            onnx_tokenizer,
        } => {
            let opts = CompressCliOptions {
                level,
                file,
                in_place,
                backup,
                llm,
                provider,
                model,
                base_url,
                format,
                ml,
                ratio,
                onnx_model,
                onnx_tokenizer,
            };
            run_compress(opts).await?;
        }
        Cmd::Stats => {
            run_stats()?;
        }
        Cmd::Gain {
            project,
            history,
            daily,
            weekly,
            monthly,
            graph,
            reset,
            yes,
            format,
        } => {
            let bucket = gain_bucket(daily, weekly, monthly)?;
            run_gain(GainOptions {
                project,
                history,
                bucket,
                graph,
                reset,
                yes,
                format,
            })?;
        }
        Cmd::Proxy { command } => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let started = std::time::Instant::now();
            let input_len = buf.len();
            let out = match rtrt_proxy::filter_for(&command) {
                Some(f) => f.apply(&buf),
                None => buf,
            };
            let mode = rtrt_proxy::filter_for(&command)
                .map(|f| f.command)
                .unwrap_or("passthrough");
            proxy_stats::record_best_effort(proxy_stats_record(
                &command,
                mode,
                input_len,
                out.len(),
                started.elapsed(),
            ));
            print!("{out}");
        }
        Cmd::ProxyRun {
            raw,
            errors_only,
            ultra_compact,
            command,
        } => run_proxy_run(command, raw, errors_only, ultra_compact),
        Cmd::Templates => {
            use rtrt_templates::TemplateCategory;
            fn rank(c: TemplateCategory) -> u8 {
                match c {
                    TemplateCategory::Development => 0,
                    TemplateCategory::Design => 1,
                    TemplateCategory::Planning => 2,
                }
            }
            let mut all = rtrt_templates::list_all();
            all.sort_by_key(|t| (rank(t.category), t.name.clone()));
            let mut current: Option<TemplateCategory> = None;
            for t in all {
                if current != Some(t.category) {
                    let label = match t.category {
                        TemplateCategory::Development => "개발 (Development)",
                        TemplateCategory::Design => "디자인 (Design)",
                        TemplateCategory::Planning => "설계 (Planning)",
                    };
                    println!("\n── {label} ──");
                    current = Some(t.category);
                }
                println!("  {:<18} [{:?}]  {}", t.name, t.source, t.description);
            }
        }
        Cmd::New {
            template,
            path,
            vars,
            overwrite,
            no_hooks,
        } => {
            let tmpl = rtrt_templates::find(&template)
                .with_context(|| format!("unknown template: {template}"))?;
            let mut map = BTreeMap::new();
            for (k, v) in vars {
                map.insert(k, v);
            }
            map.entry("project_name".into()).or_insert_with(|| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("app")
                    .to_string()
            });
            let plan = rtrt_templates::render::plan(&tmpl, &path, map)?;
            rtrt_templates::render::write(&plan, overwrite)?;
            println!(
                "scaffolded {} files into {}",
                plan.files.len(),
                plan.root.display()
            );
            if !no_hooks {
                for hook in &plan.post_hooks {
                    println!("$ {hook}");
                    run_hook(&plan.root, hook)?;
                }
            }
        }
        Cmd::Init {
            template,
            path,
            force,
            dry_run,
            vars,
        } => run_init(template, path, force, dry_run, vars)?,
        Cmd::Migrate {
            template,
            path,
            dry_run: _,
            apply,
            vars,
        } => run_migrate(template, path, apply, vars)?,
        Cmd::Project { cmd } => run_project(cmd)?,
        Cmd::Call {
            target,
            mode,
            model,
            timeout,
            format,
            failover,
            prompt,
        } => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                bail!("rtrt call: prompt is empty");
            }
            // Honor a per-project opt-out: if `<repo>/.rtrt/config.toml`
            // explicitly disables this target via `[agents]`/`[providers]`,
            // refuse to invoke it. An absent override leaves global behavior
            // unchanged.
            ensure_target_enabled(&target)?;
            let timeout = std::time::Duration::from_secs(timeout);
            if failover {
                // `--failover`: keep <target> as the primary pick, then append
                // the rest of the ranked, headroom-aware candidate list so a
                // retryable failure falls over instead of erroring out.
                run_call_with_failover(&target, mode.into(), model, timeout, &prompt, format)
                    .await?;
            } else {
                // Default single-target behavior is untouched.
                let outcome = invoke_agent(
                    &target,
                    &prompt,
                    InvokeOptions {
                        mode: Some(mode.into()),
                        model,
                        timeout,
                        provenance: None,
                    },
                )
                .await
                .with_context(|| format!("rtrt call {target}"))?;
                match format {
                    CallFormatArg::Text => print!("{}", outcome.output),
                    CallFormatArg::Json => {
                        println!("{}", serde_json::to_string_pretty(&outcome)?)
                    }
                }
            }
        }
        Cmd::Route {
            capability,
            prefer,
            target,
            model,
            mode,
            explain,
            dry_run,
            failover,
            prompt,
        } => {
            run_route(RouteCliOptions {
                capability,
                prefer,
                target,
                model,
                mode,
                explain,
                dry_run,
                failover,
                prompt,
            })
            .await?;
        }
        Cmd::Usage { format } => run_usage(format)?,
        Cmd::Provider { cmd } => run_provider(cmd).await?,
        Cmd::Memory {
            admin_legacy_store,
            cmd,
        } => run_memory(cmd, admin_legacy_store).await?,
        Cmd::Prompt { cmd } => run_prompt(cmd)?,
        Cmd::Context { cmd } => run_context(cmd)?,
        Cmd::Diagnose {
            argv,
            provider,
            model,
            base_url,
            context,
        } => {
            if argv.is_empty() {
                bail!("rtrt diagnose: command is empty");
            }
            let (bin, args) = argv.split_first().unwrap();
            let out = std::process::Command::new(bin)
                .args(args)
                .output()
                .with_context(|| format!("spawn {bin:?}"))?;
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            let errors = rtrt_proxy::errors_only(&combined, context);
            if errors.trim().is_empty() {
                println!("no failures detected; command exited {}", out.status);
                return Ok(());
            }
            eprintln!("=== captured failures ===");
            eprintln!("{errors}");
            eprintln!("=== llm diagnosis ===");
            let prov = build_provider(provider, base_url, &model)?;
            let req = ChatRequest {
                model: model.clone(),
                messages: vec![
                    ChatMessage {
                        role: Role::System,
                        content: "You are a senior engineer triaging a build / test failure. Read the captured error output and respond with: (1) one-sentence root cause; (2) the smallest concrete fix (file + change). No filler. Cite line numbers when present.".into(),
                    },
                    ChatMessage {
                        role: Role::User,
                        content: format!("Failure log:\n\n{errors}"),
                    },
                ],
                max_tokens: Some(800),
                temperature: Some(0.2),
            };
            let resp = prov.chat(req).await?;
            println!("{}", resp.content);
            eprintln!(
                "[usage] provider={} model={} input={} output={}",
                resp.provider, resp.model, resp.usage.input_tokens, resp.usage.output_tokens
            );
        }
        Cmd::Run {
            argv,
            context,
            compact,
            passthrough_status,
        } => {
            if argv.is_empty() {
                bail!("rtrt run: command is empty");
            }
            let (bin, args) = argv.split_first().unwrap();
            let out = std::process::Command::new(bin)
                .args(args)
                .output()
                .with_context(|| format!("spawn {bin:?}"))?;
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&String::from_utf8_lossy(&out.stderr));
            }
            let filtered = if compact {
                rtrt_proxy::ultra_compact(&combined)
            } else {
                rtrt_proxy::errors_only(&combined, context)
            };
            print!("{filtered}");
            if passthrough_status {
                if let Some(code) = out.status.code() {
                    std::process::exit(code);
                }
            }
        }
        Cmd::Docs {
            library,
            topic,
            base_url,
        } => {
            let client = Context7Client::new().with_base_url(base_url);
            let out = client.get_library_docs(&library, topic.as_deref()).await?;
            print!("{out}");
        }
        Cmd::Benchmark {
            bench,
            package,
            extra,
        } => {
            let mut cmd = std::process::Command::new("cargo");
            cmd.arg("bench")
                .arg("-p")
                .arg(&package)
                .arg("--bench")
                .arg(&bench);
            if !extra.is_empty() {
                cmd.arg("--");
                cmd.args(&extra);
            }
            let status = cmd
                .status()
                .map_err(|e| anyhow::anyhow!("spawn cargo: {e}"))?;
            if !status.success() {
                anyhow::bail!("cargo bench exited with {status}");
            }
            println!(
                "[rtrt benchmark] full Criterion report under target/criterion/report/index.html"
            );
        }
        Cmd::Mcp {
            transport,
            bind,
            path,
            admin_legacy_memory,
            allowed_origins,
            binary,
        } => {
            let binary = binary.unwrap_or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("rtrt-mcp")))
                    .unwrap_or_else(|| PathBuf::from("rtrt-mcp"))
            });
            let mut cmd = std::process::Command::new(&binary);
            if let Some(memory) = admin_legacy_memory {
                cmd.arg("--admin").arg("--memory").arg(memory);
            }
            cmd.arg("--transport").arg(&transport);
            if transport == "http" {
                cmd.arg("--bind").arg(&bind);
                cmd.arg("--path").arg(&path);
                if !allowed_origins.is_empty() {
                    cmd.env("RTRT_MCP_ALLOWED_ORIGINS", allowed_origins.join(","));
                }
            }
            let status = cmd
                .status()
                .map_err(|e| anyhow::anyhow!("spawn {}: {e}", binary.display()))?;
            if !status.success() {
                anyhow::bail!("rtrt-mcp exited with status {status}");
            }
        }
        Cmd::Gateway { cmd } => match cmd {
            GatewayCmd::Serve { port, host, token } => {
                serve_gateway(&host, port, token, gateway_default_timeout()).await?;
            }
        },
        Cmd::Uninstall {
            agent,
            apply,
            plugin,
        } => {
            if plugin && !matches!(agent, setup::AgentKind::Claude) {
                anyhow::bail!("--plugin is only valid with --agent claude");
            }
            setup::uninstall_agent(agent, apply)?;
            if plugin {
                setup::uninstall_claude_plugin(apply)?;
            }
        }
        Cmd::Hook { cmd } => {
            // Hook entry points must never bubble an error up to the host
            // agent, so any failure here is logged to stderr and swallowed.
            let result = match cmd {
                HookCmd::Recall {
                    project,
                    store,
                    limit,
                } => run_hook_recall(project, store, limit),
                HookCmd::Compress { project, store } => run_hook_compress(project, store).await,
                HookCmd::SessionInject {
                    project,
                    store,
                    limit,
                } => run_hook_session_inject(project, store, limit),
                HookCmd::Provenance { store, owner: _ } => run_hook_provenance(store),
                HookCmd::Style => run_hook_style(),
                HookCmd::StyleInject => run_hook_style_inject(),
                HookCmd::Statusline => {
                    print_statusline_badge();
                    Ok(())
                }
                HookCmd::ProxyRewrite => run_hook_proxy_rewrite(),
                other => run_hook_capture(other),
            };
            if let Err(e) = result {
                eprintln!("rtrt hook: {e}");
            }
        }
        Cmd::Statusline {
            rich,
            format,
            opencode,
            cwd,
            session,
            model,
            width,
            budget_ms,
            no_git,
            refresh,
        } => {
            if opencode {
                print_opencode_statusline(OpenCodeStatuslineOptions {
                    cwd,
                    session,
                    model,
                    width,
                    budget_ms,
                    no_git,
                    refresh,
                });
            } else {
                print_statusline(StatuslineOptions { rich, format });
            }
        }
        Cmd::Setup {
            agent,
            apply,
            binary,
            plugin,
            sandbox,
            no_sandbox,
            machine_only,
        } => {
            let binary = binary.unwrap_or_else(|| {
                // Best-effort: assume `rtrt-mcp` is on PATH at the same prefix as the running CLI.
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("rtrt-mcp")))
                    .unwrap_or_else(|| PathBuf::from("rtrt-mcp"))
            });
            setup::run(SetupPlan {
                agent,
                apply,
                memory_path: None,
                binary,
                plugin,
                sandbox,
                no_sandbox,
                machine_only,
            })?;
        }
        Cmd::Service { cmd } => {
            // Resolve the dashboard binary next to the running CLI (same prefix).
            let resolve_dash = |b: Option<PathBuf>| {
                b.unwrap_or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join("rtrt-dashboard")))
                        .unwrap_or_else(|| PathBuf::from("rtrt-dashboard"))
                })
            };
            let plan = match cmd {
                ServiceCmd::Open { print_bootstrap } => service::ServicePlan {
                    action: service::ServiceAction::Open { print_bootstrap },
                    apply: false,
                    binary: resolve_dash(None),
                },
                ServiceCmd::Install { apply, binary } => service::ServicePlan {
                    action: service::ServiceAction::Install,
                    apply,
                    binary: resolve_dash(binary),
                },
                ServiceCmd::Uninstall { apply } => service::ServicePlan {
                    action: service::ServiceAction::Uninstall,
                    apply,
                    binary: resolve_dash(None),
                },
                ServiceCmd::Status => service::ServicePlan {
                    action: service::ServiceAction::Status,
                    apply: false,
                    binary: resolve_dash(None),
                },
            };
            service::run(plan)?;
        }
        Cmd::Security { cmd } => security::run(cmd)?,
        Cmd::RepoMap {
            root,
            max_bytes,
            ext,
        } => {
            let restrict_ext = ext.trim();
            let mut entries: Vec<(PathBuf, String, usize, usize)> = Vec::new();
            for entry in walk_dir(&root) {
                if !entry.is_file() {
                    continue;
                }
                let name = entry.to_string_lossy();
                if !restrict_ext.is_empty() && !name.ends_with(restrict_ext) {
                    continue;
                }
                let Some(lang) = TsLanguage::from_filename(&name) else {
                    continue;
                };
                let size = std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
                if size > max_bytes {
                    continue;
                }
                let src = match std::fs::read_to_string(&entry) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let extractor = SignatureExtractor::new(lang);
                let sig = match extractor.extract(&src) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let original = src.len();
                let compressed = sig.len();
                entries.push((entry, sig, original, compressed));
            }
            // Sort by compressed size descending (rough "centrality" proxy —
            // bigger signature surface means more API).
            entries.sort_by_key(|e| std::cmp::Reverse(e.3));
            let total_before: usize = entries.iter().map(|(_, _, b, _)| b).sum();
            let total_after: usize = entries.iter().map(|(_, _, _, a)| a).sum();
            for (path, sig, before, after) in &entries {
                let rel = path.strip_prefix(&root).unwrap_or(path);
                println!(
                    "// === {} ({} → {} bytes) ===",
                    rel.display(),
                    before,
                    after
                );
                println!("{}", sig);
            }
            let pct = total_before
                .checked_sub(total_after)
                .and_then(|saved| saved.checked_mul(100))
                .and_then(|n| n.checked_div(total_before))
                .unwrap_or(0);
            eprintln!(
                "[repo-map] {} files, {} → {} bytes ({}% saved)",
                entries.len(),
                total_before,
                total_after,
                pct
            );
        }
        Cmd::Discover {
            project,
            all,
            since,
            format,
        } => run_discover(project, all, since, format)?,
        Cmd::Detect {
            format,
            kind,
            installed_only,
            enabled_only,
        } => run_detect(format, kind, installed_only, enabled_only)?,
        Cmd::Signatures { lang } => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let language = match lang.as_str() {
                "rust" | "rs" => TsLanguage::Rust,
                "python" | "py" => TsLanguage::Python,
                "ts" | "typescript" | "tsx" => TsLanguage::TypeScript,
                other => bail!("unsupported tree-sitter language: {other}"),
            };
            let out = SignatureExtractor::new(language).extract(&buf)?;
            print!("{out}");
        }
        Cmd::Info => {
            println!("rtrt v{}", env!("CARGO_PKG_VERSION"));
            println!(
                "crates: core, compress, proxy, memory, providers, templates, mcp, dashboard, cli"
            );
        }
        Cmd::Config { cmd } => run_config(cmd)?,
        Cmd::Doctor { json } => doctor::run(json)?,
    }
    Ok(())
}

const CONFIG_TEMPLATE: &str = r#"# rtrt config — ~/.rtrt/config.toml
# Every value here is a fallback: a matching RTRT_* environment variable
# always wins, so a one-off `RTRT_AUTO_COMPRESS_LLM=0 rtrt ...` still works.

[capture]
# Auto-capture pipeline (dashboard /api/* + Claude Code hooks).
enabled = true            # RTRT_AUTO_CAPTURE
redact = true             # RTRT_AUTO_REDACT — run redact_secrets before saving
dedup_window_sec = 300    # RTRT_AUTO_DEDUP_WINDOW_SEC
# project = "myproject"   # RTRT_DEFAULT_PROJECT (default: cwd basename)

[agents]
# `rtrt detect` opt-in/out. Absent key = enabled when installed.
# claude = true
# aider = false

[providers]
# Provider API opt-in/out. Never put API key values here; use environment vars.
# active = "openai"
# openrouter = false

[auto_compress]
# LLM compression of old memory rows (SessionEnd hook + dashboard daemon).
enabled = false           # RTRT_AUTO_COMPRESS_LLM — set true to turn on
model = "claude-haiku-4-5"  # RTRT_AUTO_COMPRESS_MODEL
# For a local Ollama setup, the benched recommendation is:
#   model = "gemma3:4b"
#   base_url = "http://127.0.0.1:11434/v1"
# base_url = "http://127.0.0.1:11434/v1"   # RTRT_PROVIDER_BASE_URL
interval_sec = 1800       # dashboard daemon cadence
age_sec = 3600            # RTRT_AUTO_COMPRESS_AGE_SEC — only rows older than this
min_chars = 1             # RTRT_AUTO_COMPRESS_MIN_CHARS — compress every row (raise to skip short ones)
batch = 20                # RTRT_AUTO_COMPRESS_BATCH — max rows per sweep
max_tokens = 512          # RTRT_AUTO_COMPRESS_MAX_TOKENS
"#;

const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;
const TOKEN_LOG_TRAILING_TEXT_FIELDS: usize = 2;
const TOKEN_LOG_TIMESTAMP_FIELDS: usize = 1;
const TOKEN_LOG_MIN_FIELDS: usize = TOKEN_LOG_TIMESTAMP_FIELDS + TOKEN_LOG_TRAILING_TEXT_FIELDS + 1;
const TOKEN_LOG_METRIC_LABELS: &[&str] = &[
    "input_tokens",
    "output_tokens",
    "cache_creation_tokens",
    "cache_read_tokens",
];
const SAVINGS_SOURCES: &[&str] = &["compress", "proxy"];

struct CompressCliOptions {
    /// Explicit `-l/--level`. `None` means "fall back to the repo's effective
    /// per-project compression level".
    level: Option<LevelArg>,
    file: Option<PathBuf>,
    in_place: bool,
    backup: bool,
    llm: bool,
    provider: Option<ProviderArg>,
    model: Option<String>,
    base_url: Option<String>,
    format: FormatArg,
    ml: bool,
    ratio: f32,
    onnx_model: Option<PathBuf>,
    onnx_tokenizer: Option<PathBuf>,
}

async fn run_compress(opts: CompressCliOptions) -> Result<()> {
    if opts.in_place && opts.file.is_none() {
        bail!("--in-place requires --file <PATH>");
    }
    if opts.backup && !opts.in_place {
        bail!("--backup requires --in-place");
    }

    let input = match opts.file.as_deref() {
        Some(path) => {
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
        }
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read stdin")?;
            buf
        }
    };

    let out = if opts.llm {
        let model = opts
            .model
            .ok_or_else(|| anyhow::anyhow!("--llm requires --model"))?;
        let kind = opts.provider.unwrap_or_else(|| detect_provider(&model));
        let provider = build_provider(kind, opts.base_url, &model)?;
        let compressor = LlmCompressor::new(provider, model);
        compressor.compress(&input).await?
    } else if opts.ml {
        let target = rtrt_compress::CompressionTarget::new(opts.ratio)?;
        let compressor = match (&opts.onnx_model, &opts.onnx_tokenizer) {
            #[cfg(feature = "onnx")]
            (Some(m), Some(t)) => rtrt_compress::MlCompressor::onnx(m, t)?,
            #[cfg(not(feature = "onnx"))]
            (Some(_), Some(_)) => anyhow::bail!(
                "--onnx-model requires the `onnx` cargo feature; rebuild with `cargo build --features onnx`"
            ),
            (Some(_), None) | (None, Some(_)) => {
                anyhow::bail!("--onnx-model and --onnx-tokenizer must be set together")
            }
            (None, None) => rtrt_compress::MlCompressor::heuristic(),
        };
        compressor.compress(&input, target)
    } else {
        // Level resolution: an explicit `-l/--level` always wins. Otherwise
        // fall back to the repo's *effective* per-project compression
        // (`<repo>/.rtrt/config.toml` overlaid on the global config). When the
        // project has set `[compression] enabled = false`, an unflagged
        // `rtrt compress` passes the input through unchanged.
        match opts.level {
            Some(level) => {
                let compressor = Compressor::new(level.into());
                compressor.compress_to(&input, opts.format.into())
            }
            None => {
                let compression = effective_config_for_cwd().compression;
                if compression.enabled {
                    let compressor = Compressor::new(compression.level);
                    compressor.compress_to(&input, opts.format.into())
                } else {
                    input.clone()
                }
            }
        }
    };

    if opts.in_place {
        let path = opts
            .file
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--in-place requires --file <PATH>"))?;
        if opts.backup {
            let backup = original_backup_path(path);
            std::fs::copy(path, &backup)
                .with_context(|| format!("backup {} to {}", path.display(), backup.display()))?;
        }
        std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    } else {
        print!("{out}");
    }
    Ok(())
}

fn original_backup_path(path: &std::path::Path) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(".original");
    PathBuf::from(raw)
}

struct GainOptions {
    project: Option<String>,
    history: bool,
    bucket: Option<proxy_stats::Bucket>,
    graph: bool,
    reset: bool,
    yes: bool,
    format: ReportFormatArg,
}

fn gain_bucket(daily: bool, weekly: bool, monthly: bool) -> Result<Option<proxy_stats::Bucket>> {
    let selected = [daily, weekly, monthly]
        .into_iter()
        .filter(|enabled| *enabled)
        .count();
    if selected > 1 {
        bail!("choose only one of --daily, --weekly, or --monthly");
    }
    Ok(if daily {
        Some(proxy_stats::Bucket::Daily)
    } else if weekly {
        Some(proxy_stats::Bucket::Weekly)
    } else if monthly {
        Some(proxy_stats::Bucket::Monthly)
    } else {
        None
    })
}

fn run_gain(opts: GainOptions) -> Result<()> {
    let path = proxy_stats::default_path();
    if opts.reset {
        if !opts.yes && !confirm_reset(&path)? {
            println!("reset cancelled");
            return Ok(());
        }
        proxy_stats::reset(&path)?;
        match opts.format {
            ReportFormatArg::Json => {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "reset",
                        "path": path.display().to_string(),
                    })
                );
            }
            ReportFormatArg::Table => println!("reset {}", path.display()),
        }
        return Ok(());
    }

    let bucket = opts
        .bucket
        .or(opts.graph.then_some(proxy_stats::Bucket::Daily));
    let summary = proxy_stats::load_summary(opts.project.as_deref(), bucket, opts.history)?;
    match opts.format {
        ReportFormatArg::Json => print_gain_json(&summary, opts.graph)?,
        ReportFormatArg::Table => print_gain_table(&summary, opts.graph),
    }
    Ok(())
}

fn confirm_reset(path: &std::path::Path) -> Result<bool> {
    eprint!(
        "Clear Command Optimizer stats at {}? Type yes to continue: ",
        path.display()
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("yes"))
}

fn print_gain_table(summary: &proxy_stats::GainSummary, graph: bool) {
    println!("Command Optimizer stats");
    println!("db: {}", summary.path.display());
    if let Some(reason) = &summary.unavailable {
        println!("db_status: unavailable ({reason})");
    }
    println!("total runs: {}", summary.total_runs);
    println!("total saved chars: {}", summary.saved_chars);
    println!(
        "estimated tokens: {} (estimate, chars/{ESTIMATED_CHARS_PER_TOKEN})",
        estimated_tokens(summary.saved_chars)
    );
    println!("input chars: {}", summary.input_chars);
    println!("output chars: {}", summary.output_chars);

    println!();
    println!("top commands by savings:");
    if summary.top_commands.is_empty() {
        println!("  (none)");
    } else {
        for row in &summary.top_commands {
            println!(
                "  {}  runs={} saved_chars={} estimated_tokens={}",
                row.command,
                row.runs,
                row.saved_chars,
                estimated_tokens(row.saved_chars)
            );
        }
    }

    println!();
    println!("per-project breakdown:");
    if summary.projects.is_empty() {
        println!("  (none)");
    } else {
        for row in &summary.projects {
            println!(
                "  {}  runs={} saved_chars={} estimated_tokens={}",
                row.project,
                row.runs,
                row.saved_chars,
                estimated_tokens(row.saved_chars)
            );
        }
    }

    if !summary.buckets.is_empty() {
        println!();
        println!("bucketed totals:");
        for row in &summary.buckets {
            println!(
                "  {}  runs={} saved_chars={} estimated_tokens={}",
                row.bucket,
                row.runs,
                row.saved_chars,
                estimated_tokens(row.saved_chars)
            );
        }
    }

    if graph {
        println!();
        println!("savings graph:");
        print_gain_graph(&summary.buckets);
    }

    if !summary.recent.is_empty() {
        println!();
        println!("recent runs:");
        for row in &summary.recent {
            println!(
                "  {}  {}  {}  mode={} {}->{} saved={} ({:.1}%) exec_ms={}",
                row.ts,
                row.project,
                row.original_cmd,
                row.mode,
                row.input_chars,
                row.output_chars,
                row.saved_chars,
                row.saved_pct,
                row.exec_ms
            );
        }
    }
}

fn print_gain_json(summary: &proxy_stats::GainSummary, graph: bool) -> Result<()> {
    let value = serde_json::json!({
        "db": summary.path.display().to_string(),
        "db_status": summary.unavailable.as_ref().map(|reason| serde_json::json!({
            "status": "unavailable",
            "reason": reason,
        })),
        "total_runs": summary.total_runs,
        "total_saved_chars": summary.saved_chars,
        "estimated_tokens": estimated_tokens(summary.saved_chars),
        "token_estimate": format!("chars/{ESTIMATED_CHARS_PER_TOKEN}"),
        "input_chars": summary.input_chars,
        "output_chars": summary.output_chars,
        "exec_ms": summary.exec_ms,
        "top_commands": summary.top_commands.iter().map(|row| serde_json::json!({
            "command": row.command,
            "runs": row.runs,
            "saved_chars": row.saved_chars,
            "estimated_tokens": estimated_tokens(row.saved_chars),
        })).collect::<Vec<_>>(),
        "projects": summary.projects.iter().map(|row| serde_json::json!({
            "project": row.project,
            "runs": row.runs,
            "saved_chars": row.saved_chars,
            "estimated_tokens": estimated_tokens(row.saved_chars),
        })).collect::<Vec<_>>(),
        "buckets": summary.buckets.iter().map(|row| serde_json::json!({
            "bucket": row.bucket,
            "runs": row.runs,
            "saved_chars": row.saved_chars,
            "estimated_tokens": estimated_tokens(row.saved_chars),
        })).collect::<Vec<_>>(),
        "graph": graph.then(|| gain_graph_lines(&summary.buckets)),
        "recent": summary.recent.iter().map(|row| serde_json::json!({
            "ts": row.ts,
            "project": row.project,
            "original_cmd": row.original_cmd,
            "mode": row.mode,
            "input_chars": row.input_chars,
            "output_chars": row.output_chars,
            "saved_chars": row.saved_chars,
            "saved_pct": row.saved_pct,
            "exec_ms": row.exec_ms,
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn print_gain_graph(buckets: &[proxy_stats::BucketSavings]) {
    let lines = gain_graph_lines(buckets);
    if lines.is_empty() {
        println!("  (none)");
    } else {
        for line in lines {
            println!("  {line}");
        }
    }
}

fn gain_graph_lines(buckets: &[proxy_stats::BucketSavings]) -> Vec<String> {
    let max_saved = buckets.iter().map(|row| row.saved_chars).max().unwrap_or(0);
    if max_saved == 0 {
        return Vec::new();
    }
    let max_width = proxy_stats::derived_count(max_saved);
    buckets
        .iter()
        .map(|row| {
            let width =
                ((row.saved_chars as usize).saturating_mul(max_width) / max_saved as usize).max(1);
            format!("{} | {} {}", row.bucket, "#".repeat(width), row.saved_chars)
        })
        .collect()
}

#[derive(Default)]
struct DiscoverReport {
    sessions_scanned: usize,
    total_commands: usize,
    supported: usize,
    unsupported: usize,
    estimated_savings_tokens: u64,
}

fn run_discover(
    project: Option<String>,
    all: bool,
    since: Option<String>,
    format: ReportFormatArg,
) -> Result<()> {
    let project_filter = if all {
        None
    } else {
        Some(project.unwrap_or_else(current_project_name))
    };
    let averages = proxy_stats::load_savings_averages();
    let mut report = DiscoverReport::default();
    scan_claude_transcripts(
        project_filter.as_deref(),
        since.as_deref(),
        &averages,
        &mut report,
    );
    if all {
        scan_shell_history(since.as_deref(), &averages, &mut report);
    }
    match format {
        ReportFormatArg::Json => {
            let value = serde_json::json!({
                "sessions_scanned": report.sessions_scanned,
                "total_commands": report.total_commands,
                "supported": report.supported,
                "unsupported": report.unsupported,
                "estimated_savings_tokens": report.estimated_savings_tokens,
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ReportFormatArg::Table => {
            println!("Command Optimizer discovery");
            println!("sessions_scanned: {}", report.sessions_scanned);
            println!("total_commands: {}", report.total_commands);
            println!("supported: {}", report.supported);
            println!("unsupported: {}", report.unsupported);
            println!(
                "estimated_savings_tokens: {} (estimate, chars/{ESTIMATED_CHARS_PER_TOKEN})",
                report.estimated_savings_tokens
            );
        }
    }
    Ok(())
}

fn scan_claude_transcripts(
    project_filter: Option<&str>,
    since: Option<&str>,
    averages: &proxy_stats::SavingsAverages,
    report: &mut DiscoverReport,
) {
    let Some(root) = claude_projects_dir() else {
        return;
    };
    let Ok(project_dirs) = std::fs::read_dir(root) else {
        return;
    };
    for project_dir in project_dirs.filter_map(std::result::Result::ok) {
        let Ok(files) = std::fs::read_dir(project_dir.path()) else {
            continue;
        };
        for file in files.filter_map(std::result::Result::ok) {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            report.sessions_scanned = report.sessions_scanned.saturating_add(1);
            scan_transcript_file(&path, project_filter, since, averages, report);
        }
    }
}

fn scan_transcript_file(
    path: &std::path::Path,
    project_filter: Option<&str>,
    since: Option<&str>,
    averages: &proxy_stats::SavingsAverages,
    report: &mut DiscoverReport,
) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if !entry_matches_since(&value, since) {
            continue;
        }
        let project = value
            .get("cwd")
            .and_then(|cwd| cwd.as_str())
            .map(rtrt_core::project_for_cwd_str)
            .filter(|name| !name.is_empty());
        if let Some(wanted) = project_filter {
            if project.as_deref() != Some(wanted) {
                continue;
            }
        }
        for command in extract_bash_commands(&value) {
            record_discovered_command(&command, averages, report);
        }
    }
}

fn entry_matches_since(value: &serde_json::Value, since: Option<&str>) -> bool {
    let Some(since) = since else {
        return true;
    };
    value
        .get("timestamp")
        .and_then(|timestamp| timestamp.as_str())
        .is_some_and(|timestamp| timestamp >= since)
}

fn extract_bash_commands(value: &serde_json::Value) -> Vec<String> {
    let mut commands = Vec::new();
    if value.get("tool_name").and_then(|name| name.as_str()) == Some("Bash") {
        if let Some(command) = value
            .get("tool_input")
            .and_then(|input| input.get("command"))
            .and_then(|command| command.as_str())
        {
            commands.push(command.to_string());
        }
    }
    if let Some(blocks) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_array())
    {
        for block in blocks {
            if block.get("name").and_then(|name| name.as_str()) != Some("Bash") {
                continue;
            }
            if let Some(command) = block
                .get("input")
                .and_then(|input| input.get("command"))
                .and_then(|command| command.as_str())
            {
                commands.push(command.to_string());
            }
        }
    }
    commands
}

fn record_discovered_command(
    command: &str,
    averages: &proxy_stats::SavingsAverages,
    report: &mut DiscoverReport,
) {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return;
    }
    report.total_commands = report.total_commands.saturating_add(1);
    let first = first_whitespace_token(trimmed);
    let filter = rtrt_proxy::filter_for(trimmed).or_else(|| first.and_then(rtrt_proxy::filter_for));
    let supported = filter.is_some()
        || first
            .map(|token| KNOWN_SHRINKABLE_COMMANDS.contains(&token))
            .unwrap_or(false);
    if supported {
        report.supported = report.supported.saturating_add(1);
        let saved_chars = averages.estimate_for(trimmed, filter.map(|f| f.command), first);
        report.estimated_savings_tokens = report
            .estimated_savings_tokens
            .saturating_add(estimated_tokens(saved_chars));
    } else {
        report.unsupported = report.unsupported.saturating_add(1);
    }
}

fn scan_shell_history(
    since: Option<&str>,
    averages: &proxy_stats::SavingsAverages,
    report: &mut DiscoverReport,
) {
    let Some(path) = default_history_path() else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    for line in raw.lines() {
        let (timestamp, command) = parse_history_line(line);
        if since.is_some() && timestamp.is_none() {
            continue;
        }
        if let (Some(ts), Some(since)) = (timestamp, since) {
            if ts < since {
                continue;
            }
        }
        record_discovered_command(command, averages, report);
    }
}

fn parse_history_line(line: &str) -> (Option<&str>, &str) {
    if let Some(rest) = line.strip_prefix(": ") {
        if let Some((head, command)) = rest.split_once(';') {
            let ts = head
                .split(':')
                .next()
                .map(str::trim)
                .filter(|ts| !ts.is_empty());
            return (ts, command.trim());
        }
    }
    (None, line.trim())
}

fn claude_projects_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(".claude").join("projects"))
}

fn run_stats() -> Result<()> {
    run_gain(GainOptions {
        project: None,
        history: false,
        bucket: None,
        graph: false,
        reset: false,
        yes: true,
        format: ReportFormatArg::Table,
    })?;
    println!();
    println!("Output Optimizer stats");
    print_token_log_stats()?;
    print_memory_savings();
    Ok(())
}

#[derive(Debug)]
struct TokenLogRow {
    timestamp: u64,
    metrics: Vec<u64>,
    model: String,
    session_id: String,
}

fn print_token_log_stats() -> Result<()> {
    let path = PathBuf::from(".priv-storage")
        .join("sessions")
        .join("token-log.tsv");
    if !path.exists() {
        println!("token-log: unavailable ({} not found)", path.display());
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < TOKEN_LOG_MIN_FIELDS {
            skipped += 1;
            continue;
        }
        let Ok(timestamp) = fields[0].parse::<u64>() else {
            skipped += 1;
            continue;
        };
        let metric_end = fields.len().saturating_sub(TOKEN_LOG_TRAILING_TEXT_FIELDS);
        let mut metrics = Vec::new();
        let mut valid = true;
        for value in &fields[TOKEN_LOG_TIMESTAMP_FIELDS..metric_end] {
            match value.parse::<u64>() {
                Ok(n) => metrics.push(n),
                Err(_) => {
                    valid = false;
                    break;
                }
            }
        }
        if !valid || metrics.is_empty() {
            skipped += 1;
            continue;
        }
        rows.push(TokenLogRow {
            timestamp,
            metrics,
            model: fields[fields.len() - 2].to_string(),
            session_id: fields[fields.len() - 1].to_string(),
        });
    }

    let Some(latest) = rows.iter().max_by_key(|row| row.timestamp) else {
        println!(
            "token-log: unavailable ({} had no parseable rows)",
            path.display()
        );
        if skipped > 0 {
            println!("token-log skipped rows: {skipped}");
        }
        return Ok(());
    };
    let session_rows = rows
        .iter()
        .filter(|row| row.session_id == latest.session_id)
        .count();
    let latest_total: u64 = latest.metrics.iter().sum();
    println!("token-log: available ({})", path.display());
    println!(
        "  latest session: {} model={} rows={}",
        latest.session_id, latest.model, session_rows
    );
    for (idx, value) in latest.metrics.iter().enumerate() {
        let label = TOKEN_LOG_METRIC_LABELS
            .get(idx)
            .copied()
            .unwrap_or("metric");
        if label == "metric" {
            println!("  metric_{}: {}", idx + 1, value);
        } else {
            println!("  {label}: {value}");
        }
    }
    println!("  latest total tokens: {latest_total}");
    if skipped > 0 {
        println!("  skipped rows: {skipped}");
    }
    Ok(())
}

#[derive(Clone, Default)]
struct SourceSavings {
    saved_chars: u64,
    rows: usize,
}

fn print_memory_savings() {
    let Ok(identity) = current_project_identity() else {
        println!("memory: unavailable (project identity unavailable)");
        return;
    };
    let Ok(path) = rtrt_core::project_memory_db_path(&identity) else {
        println!("memory: unavailable (project store unavailable)");
        return;
    };
    if !path.exists() {
        println!("memory: unavailable ({} not found)", path.display());
        println!("savings: unavailable (memory store unavailable)");
        for source in SAVINGS_SOURCES {
            println!("  {source}: unavailable");
        }
        return;
    }
    let store = match MemoryStore::open_project(&identity) {
        Ok(store) => store,
        Err(e) => {
            println!("memory: unavailable ({}: {e})", path.display());
            println!("savings: unavailable (memory store unavailable)");
            for source in SAVINGS_SOURCES {
                println!("  {source}: unavailable");
            }
            return;
        }
    };
    let projects = match store.count_by_project(identity.slug()) {
        Ok(count) => vec![(identity.slug().to_string(), count, 0_i64)],
        Err(e) => {
            println!("memory: unavailable ({}: {e})", path.display());
            println!("savings: unavailable (memory metadata query failed)");
            for source in SAVINGS_SOURCES {
                println!("  {source}: unavailable");
            }
            return;
        }
    };

    let mut by_source: BTreeMap<String, SourceSavings> = SAVINGS_SOURCES
        .iter()
        .map(|source| (source.to_string(), SourceSavings::default()))
        .collect();
    let mut metadata_errors = 0usize;
    let mut invalid_saved_chars = 0usize;
    for (project, count, _) in &projects {
        let rows = match store.list_by_project(project, *count) {
            Ok(rows) => rows,
            Err(_) => {
                metadata_errors += *count;
                continue;
            }
        };
        for row in rows {
            let meta = match store.get_metadata(row.id) {
                Ok(meta) => meta,
                Err(_) => {
                    metadata_errors += 1;
                    continue;
                }
            };
            let Some(source) = meta.get("source").map(String::as_str) else {
                continue;
            };
            if !SAVINGS_SOURCES.contains(&source) {
                continue;
            }
            let Some(saved) = meta
                .get("saved_chars")
                .and_then(|value| parse_saved_chars(value))
            else {
                invalid_saved_chars += 1;
                continue;
            };
            if let Some(stats) = by_source.get_mut(source) {
                stats.saved_chars = stats.saved_chars.saturating_add(saved);
                stats.rows = stats.rows.saturating_add(1);
            }
        }
    }

    println!("memory: available ({})", path.display());
    println!("savings (tokens estimate ~= chars/{ESTIMATED_CHARS_PER_TOKEN}):");
    let mut total_chars = 0u64;
    let mut total_rows = 0usize;
    for source in SAVINGS_SOURCES {
        let stats = by_source.get(*source).cloned().unwrap_or_default();
        total_chars = total_chars.saturating_add(stats.saved_chars);
        total_rows = total_rows.saturating_add(stats.rows);
        println!(
            "  {source}: {} chars ~= {} tokens ({} rows)",
            stats.saved_chars,
            estimated_tokens(stats.saved_chars),
            stats.rows
        );
    }
    println!(
        "  total: {} chars ~= {} tokens ({} rows)",
        total_chars,
        estimated_tokens(total_chars),
        total_rows
    );
    if metadata_errors > 0 {
        println!("  metadata rows unavailable: {metadata_errors}");
    }
    if invalid_saved_chars > 0 {
        println!("  rows with invalid saved_chars: {invalid_saved_chars}");
    }
}

fn parse_saved_chars(value: &str) -> Option<u64> {
    value
        .parse::<i64>()
        .ok()
        .and_then(|n| u64::try_from(n.max(0)).ok())
}

fn estimated_tokens(chars: u64) -> u64 {
    chars / ESTIMATED_CHARS_PER_TOKEN
}

fn run_config(cmd: ConfigCmd) -> Result<()> {
    let path = rtrt_core::Config::default_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve config path (no HOME?)"))?;
    match cmd {
        ConfigCmd::Path => {
            println!(
                "{} ({})",
                path.display(),
                if path.exists() { "exists" } else { "absent" }
            );
        }
        ConfigCmd::Init { force } => {
            if path.exists() && !force {
                anyhow::bail!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                );
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, CONFIG_TEMPLATE)?;
            println!("wrote {}", path.display());
            // Validate it parses so a shipped template can't be broken.
            rtrt_core::Config::load()?;
            println!("ok — edit it, then `rtrt config path` to confirm");
        }
    }
    Ok(())
}

fn run_detect(
    format: DetectFormatArg,
    kind: Option<DetectKindArg>,
    installed_only: bool,
    enabled_only: bool,
) -> Result<()> {
    let selected_kind = kind.map(ToolKind::from);
    // Detect with the repo's effective config so the `enabled` column reflects
    // per-project `[agents]`/`[providers]` overrides (global when no repo).
    let mut tools = rtrt_core::detect_tools_with_config(effective_config_for_cwd());
    tools.retain(|tool| {
        selected_kind.is_none_or(|kind| tool.kind == kind)
            && (!installed_only || tool.installed)
            && (!enabled_only || tool.enabled)
    });

    match format {
        DetectFormatArg::Json => {
            println!("{}", serde_json::to_string_pretty(&tools)?);
        }
        DetectFormatArg::Table => print_detect_table(&tools),
    }
    Ok(())
}

fn print_detect_table(tools: &[DetectedTool]) {
    let mut printed_group = false;
    for kind in DETECT_KIND_ORDER {
        let group = tools
            .iter()
            .filter(|tool| tool.kind == *kind)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        if printed_group {
            println!();
        }
        println!("{}", detect_kind_label(*kind));
        println!(
            "{:<name_w$} | {:<installed_w$} | {:<version_w$} | {:<modes_w$} | {:<cost_w$} | {:<enabled_w$} | invocation/models",
            "name",
            "installed",
            "version",
            "modes",
            "cost",
            "enabled",
            name_w = DETECT_NAME_WIDTH,
            installed_w = DETECT_INSTALLED_WIDTH,
            version_w = DETECT_VERSION_WIDTH,
            modes_w = DETECT_MODES_WIDTH,
            cost_w = DETECT_COST_WIDTH,
            enabled_w = DETECT_ENABLED_WIDTH,
        );
        for tool in group {
            println!(
                "{:<name_w$} | {:<installed_w$} | {:<version_w$} | {:<modes_w$} | {:<cost_w$} | {:<enabled_w$} | {}",
                tool.name,
                bool_label(tool.installed),
                compact_cell(tool.version.as_deref().unwrap_or("-"), DETECT_VERSION_WIDTH),
                invocation_modes_label(&tool.invocation_modes),
                cost_class_label(tool.cost_class),
                bool_label(tool.enabled),
                compact_cell(&detect_detail(tool), DETECT_DETAIL_WIDTH),
                name_w = DETECT_NAME_WIDTH,
                installed_w = DETECT_INSTALLED_WIDTH,
                version_w = DETECT_VERSION_WIDTH,
                modes_w = DETECT_MODES_WIDTH,
                cost_w = DETECT_COST_WIDTH,
                enabled_w = DETECT_ENABLED_WIDTH,
            );
        }
        printed_group = true;
    }
}

fn detect_kind_label(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::CodingAgent => "coding-agent",
        ToolKind::LocalRuntime => "local-runtime",
        ToolKind::ProviderApi => "provider-api",
        ToolKind::McpServer => "mcp-server",
    }
}

fn invocation_modes_label(modes: &[InvocationMode]) -> String {
    modes
        .iter()
        .map(|mode| match mode {
            InvocationMode::Cli => "cli",
            InvocationMode::Api => "api",
            InvocationMode::Mcp => "mcp",
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn cost_class_label(cost: CostClass) -> &'static str {
    match cost {
        CostClass::LocalFree => "local-free",
        CostClass::SubscriptionFlat => "subscription-flat",
        CostClass::ApiMetered => "api-metered",
        CostClass::Unknown => "unknown",
    }
}

fn invoke_mode_label(mode: InvokeMode) -> &'static str {
    match mode {
        InvokeMode::Cli => "cli",
        InvokeMode::Api => "api",
        InvokeMode::Auto => "auto",
    }
}

fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn detect_detail(tool: &DetectedTool) -> String {
    if !tool.models.is_empty() {
        return format!("models: {}", tool.models.join(","));
    }
    tool.cli_invocation
        .clone()
        .or_else(|| tool.path.clone())
        .unwrap_or_else(|| "-".to_string())
}

fn compact_cell(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compact = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{compact}...")
    } else {
        compact
    }
}

fn run_hook(cwd: &std::path::Path, hook: &str) -> Result<()> {
    let parts: Vec<&str> = hook.split_whitespace().collect();
    let Some((bin, args)) = parts.split_first() else {
        return Ok(());
    };
    let status = std::process::Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .status()?;
    if !status.success() {
        bail!("hook `{hook}` exited with {status}");
    }
    Ok(())
}

struct RouteCliOptions {
    capability: Option<RouteCapabilityArg>,
    prefer: RoutePreferArg,
    target: Option<String>,
    model: Option<String>,
    mode: CallModeArg,
    explain: bool,
    dry_run: bool,
    failover: bool,
    prompt: Vec<String>,
}

/// The routing request behind one `rtrt route` invocation.
///
/// `--target` wins; otherwise the per-project `[providers] active` preference
/// pins the route. `--failover` rides along with it instead of being dropped:
/// an explicit target used to collapse the route to a single candidate, so
/// merely configuring `active` turned `--failover` into a no-op. The router now
/// keeps a pinned target FIRST and ranks its sibling pools and the other
/// targets behind it, so the flag means the same thing pinned or not. Without
/// `--failover` the request is byte-identical to the one this always built.
fn route_request_for(opts: &RouteCliOptions, active: Option<&str>) -> RouteRequest {
    let mode = InvokeMode::from(opts.mode);
    RouteRequest {
        capability: opts.capability.map(Capability::from),
        prefer: Prefer::from(opts.prefer),
        target: opts.target.clone().or_else(|| active.map(str::to_string)),
        model: opts.model.clone(),
        mode: (mode != InvokeMode::Auto).then_some(mode),
        failover: opts.failover,
    }
}

async fn run_route(opts: RouteCliOptions) -> Result<()> {
    let prompt = opts.prompt.join(" ");
    // `--explain` / `--dry-run` only print the decision, so they don't need a
    // prompt. A prompt is required only when we actually invoke the target.
    let will_invoke = !opts.explain && !opts.dry_run;
    if will_invoke && prompt.trim().is_empty() {
        bail!("rtrt route: prompt is empty");
    }
    // Effective config = global overlaid with `<repo>/.rtrt/config.toml`. It
    // drives two things: the per-project `[agents]`/`[providers]` enable map
    // (fed into detection so disabled targets drop out of the candidate set),
    // and the per-project `[providers] active` preference (used as the route
    // target when the user gave no explicit `--target`).
    let cfg = effective_config_for_cwd();
    let req = route_request_for(&opts, cfg.providers.active.as_deref());
    let tools = rtrt_core::detect_tools_with_config(cfg);
    // Routing snapshot with the ledger's rolling 24h window so ranking is
    // headroom-aware: exhausted `[limits]` targets are demoted and near-limit
    // ones penalized (targets with no cap keep pure cost-tier order).
    let usage = UsageSnapshot::load_for_routing();
    let decision = select_route(&req, &tools, &usage)?;

    if opts.explain || opts.dry_run {
        // Windowed ledger view + per-target headroom against the effective
        // `[limits]` caps, surfaced per candidate in `--explain`.
        let ledger_cfg = effective_config_for_cwd();
        let windows = provider_usage_windows();
        let headroom = target_headroom(&ledger_cfg);
        let pools = candidate_pools(&decision, &ledger_cfg);
        print_route_decision(&decision, opts.explain, &usage, &windows, &headroom, &pools);
    }
    // Stop before invoking on `--dry-run`, or on `--explain` with no prompt
    // (decision-only inspection).
    if opts.dry_run || prompt.trim().is_empty() {
        return Ok(());
    }

    let timeout = std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    // `--failover`: walk the ranked candidate list, falling over on retryable
    // failures. A pinned target (explicit `--target` or the configured
    // `[providers] active`) stays the primary pick and keeps its ranked
    // fallbacks behind it, so pinning no longer silently disarms the flag.
    if opts.failover {
        let ranked = decision.ranked_targets();
        let result = invoke_with_failover(&ranked, &prompt, timeout)
            .await
            .with_context(|| format!("rtrt route --failover ({} candidate(s))", ranked.len()))?;
        eprintln!("route: {}", result.summary());
        print!("{}", result.outcome.output);
        return Ok(());
    }

    let outcome = invoke_agent(
        &decision.target,
        &prompt,
        InvokeOptions {
            mode: Some(decision.mode),
            model: decision.model.clone(),
            timeout,
            provenance: None,
        },
    )
    .await
    .with_context(|| format!("rtrt route {}", decision.target))?;
    print!("{}", outcome.output);
    Ok(())
}

/// `rtrt call <target> --failover`: invoke `<target>` first, then fall over
/// through the rest of the headroom-aware ranked list on retryable failures.
async fn run_call_with_failover(
    target: &str,
    mode: InvokeMode,
    model: Option<String>,
    timeout: std::time::Duration,
    prompt: &str,
    format: CallFormatArg,
) -> Result<()> {
    let ranked = ranked_targets_for_call(target, mode, model);
    let result = invoke_with_failover(&ranked, prompt, timeout)
        .await
        .with_context(|| format!("rtrt call {target} --failover"))?;
    eprintln!("call: {}", result.summary());
    match format {
        CallFormatArg::Text => print!("{}", result.outcome.output),
        CallFormatArg::Json => println!("{}", serde_json::to_string_pretty(&result)?),
    }
    Ok(())
}

/// Build the failover order for `rtrt call <target> --failover`: the explicit
/// target stays the primary pick, then the remaining ranked, headroom-aware
/// candidates are appended behind it.
///
/// The pin is handed to the router as an explicit target *with* failover, so the
/// primary resolves to that target's roomiest pool and its sibling pools rank
/// ahead of the other targets — a pinned call now survives one upstream quota
/// running out. A target the router cannot rank (a custom binary, say) falls
/// back to the previous shape: the caller's own primary, then the open ranking
/// behind it, dropping the primary if it reappears.
fn ranked_targets_for_call(
    target: &str,
    mode: InvokeMode,
    model: Option<String>,
) -> Vec<RankedTarget> {
    let cfg = effective_config_for_cwd();
    let tools = rtrt_core::detect_tools_with_config(cfg);
    let usage = UsageSnapshot::load_for_routing();
    let req = RouteRequest {
        capability: None,
        prefer: Prefer::Cheapest,
        target: Some(target.to_string()),
        model: model.clone(),
        mode: (mode != InvokeMode::Auto).then_some(mode),
        failover: true,
    };
    if let Ok(decision) = select_route(&req, &tools, &usage) {
        return decision.ranked_targets();
    }
    let primary = RankedTarget {
        target: target.to_string(),
        mode,
        model,
        cost_class: CostClass::Unknown,
    };
    let open = RouteRequest {
        target: None,
        model: None,
        failover: false,
        ..req
    };
    let mut ranked = vec![primary];
    if let Ok(decision) = select_route(&open, &tools, &usage) {
        let normalized = target.to_ascii_lowercase();
        for candidate in decision.ranked_targets() {
            if candidate.target.to_ascii_lowercase() != normalized {
                ranked.push(candidate);
            }
        }
    }
    ranked
}

fn print_route_decision(
    decision: &RouteDecision,
    explain: bool,
    usage: &UsageSnapshot,
    windows: &BTreeMap<String, TargetWindows>,
    headroom: &BTreeMap<String, TargetHeadroom>,
    pools: &[Option<CandidatePool>],
) {
    println!("target: {}", decision.target);
    println!("mode: {}", invoke_mode_label(decision.mode));
    println!("model: {}", decision.model.as_deref().unwrap_or("-"));
    println!("cost: {}", cost_class_label(decision.cost_class));
    println!("reason: {}", decision.reason);
    if !explain {
        return;
    }
    // Per-candidate recent usage + remaining headroom from the ledger. The
    // chosen target is listed first, then each ranked alternative. A candidate
    // whose model names an upstream pool is reported per pool, because that —
    // not the target — is the quota bucket the call actually draws down.
    println!("candidates:");
    let mut pools = pools.iter();
    print_candidate_headroom(
        "* ",
        &decision.target,
        windows,
        headroom,
        pools.next().and_then(Option::as_ref),
    );
    for alt in &decision.alternatives {
        print_candidate_headroom(
            "  ",
            &alt.target,
            windows,
            headroom,
            pools.next().and_then(Option::as_ref),
        );
    }
    println!("alternatives:");
    if decision.alternatives.is_empty() {
        println!("  (none)");
    } else {
        for alt in &decision.alternatives {
            println!(
                "  {} mode={} model={} cost={} headroom={} reason={}",
                alt.target,
                invoke_mode_label(alt.mode),
                alt.model.as_deref().unwrap_or("-"),
                cost_class_label(alt.cost_class),
                alt.headroom,
                alt.reason
            );
        }
    }
    println!("usage:");
    if usage.usage_by_target.is_empty() {
        println!("  tokens: unknown");
    } else {
        for (target, tokens) in &usage.usage_by_target {
            println!("  {target}: used_tokens={tokens}");
        }
    }
    if usage.limits_by_target.is_empty() {
        println!("  limits: unknown");
    } else {
        for (target, limit) in &usage.limits_by_target {
            let used = usage.usage_by_target.get(target).copied().unwrap_or(0);
            println!(
                "  {target}: limit={limit} remaining={}",
                limit.saturating_sub(used)
            );
        }
    }
    if let Some(proxy) = usage.proxy_runs {
        println!(
            "  proxy: runs={} input_chars={} output_chars={}",
            proxy.runs, proxy.input_chars, proxy.output_chars
        );
    }
    println!("sources:");
    for source in &usage.sources {
        println!("  {source}");
    }
}

/// The quota bucket behind one pooled candidate, plus what an ordering over the
/// pooled candidates was actually derived from.
///
/// Only built for a candidate whose model names a pool: an unpooled candidate
/// has no identity finer than its target, so it keeps printing the target line
/// it always printed.
struct CandidatePool {
    key: PoolKey,
    room: PoolHeadroom,
    basis: RoomBasis,
}

/// Resolve the pool identity and room of every ranked candidate — the pick
/// first, then each alternative — in the order `print_route_decision` lists
/// them. `None` marks an unpooled candidate.
///
/// The basis is shared by the whole set, because that is what the ordering was
/// decided on: [`RoomBasis::Quota`] only when every pooled candidate has a
/// configured cap, otherwise the order fell back to observed usage.
fn candidate_pools(
    decision: &RouteDecision,
    cfg: &rtrt_core::Config,
) -> Vec<Option<CandidatePool>> {
    let keys: Vec<PoolKey> = std::iter::once((decision.target.as_str(), decision.model.as_deref()))
        .chain(
            decision
                .alternatives
                .iter()
                .map(|alt| (alt.target.as_str(), alt.model.as_deref())),
        )
        .map(|(target, model)| PoolKey::from_target_model(target, model))
        .collect();
    if !keys.iter().any(PoolKey::is_pooled) {
        // No pools in play at all: nothing to disclose, and every line stays
        // exactly what it was before pools existed.
        return keys.iter().map(|_| None).collect();
    }
    let rooms: Vec<Option<PoolHeadroom>> = keys
        .iter()
        .map(|key| key.is_pooled().then(|| headroom_for_pool(key, cfg)))
        .collect();
    let basis = rank_pools_by_room(&rooms.iter().flatten().cloned().collect::<Vec<_>>()).basis;
    keys.into_iter()
        .zip(rooms)
        .map(|(key, room)| room.map(|room| CandidatePool { key, room, basis }))
        .collect()
}

/// One candidate's recent (24h) usage and remaining headroom, formatted for
/// `route --explain`. The `~` prefix on a token count marks an estimate (CLI
/// shell-outs); a `?` for headroom means no `[limits]` cap is configured.
///
/// A pooled candidate is labelled `target#pool` and reports that pool's own
/// numbers, so the line can never be read as the target's whole allowance. When
/// the pooled ordering is [`RoomBasis::ObservedUsage`] the line says so: without
/// a configured cap the remaining quota is unknowable, and a least-used-first
/// order must never be presented as a quota measurement.
fn print_candidate_headroom(
    marker: &str,
    target: &str,
    windows: &BTreeMap<String, TargetWindows>,
    headroom: &BTreeMap<String, TargetHeadroom>,
    pool: Option<&CandidatePool>,
) {
    if let Some(pool) = pool {
        let used = format_token_count(pool.room.used_tokens, pool.room.tokens_estimated);
        println!(
            "{marker}{}: 24h used={used} requests={} remaining={}{}",
            pool.key.canonical(),
            pool.room.used_requests,
            format_pool_room(&pool.room),
            pool_basis_suffix(pool.basis),
        );
        return;
    }
    let normalized = target.to_ascii_lowercase();
    let window = windows.get(&normalized).copied().unwrap_or_default();
    let recent = window.last_24h;
    let used = format_token_count(recent.tokens, recent.has_estimates());
    let head = headroom
        .get(&normalized)
        .map(format_headroom)
        .unwrap_or_else(|| "?".to_string());
    println!(
        "{marker}{target}: 24h used={used} requests={} remaining={head}",
        recent.requests
    );
}

/// Remaining room for one pool. `?` when no cap is configured at either level —
/// a ceiling is never invented — and a cap inherited from the target is marked
/// as shared with the sibling pools instead of being reported as this pool's
/// own allowance.
fn format_pool_room(room: &PoolHeadroom) -> String {
    if room.limits_unknown() {
        return "? (no [limits] cap)".to_string();
    }
    let mut parts = Vec::new();
    if let Some(tokens) = format_pool_cap(room.tokens, "tok") {
        parts.push(tokens);
    }
    if let Some(requests) = format_pool_cap(room.requests, "req") {
        parts.push(requests);
    }
    if parts.is_empty() {
        return "?".to_string();
    }
    let mut label = parts.join(", ");
    if room.shares_a_cap() {
        label.push_str(" (cap shared with sibling pools)");
    }
    label
}

/// One capped axis, or `None` when that axis has no configured ceiling.
fn format_pool_cap(cap: PoolCap, unit: &str) -> Option<String> {
    let limit = cap.limit?;
    Some(format!(
        "{}/{limit} {unit} left (used {})",
        cap.remaining.unwrap_or_default(),
        cap.used
    ))
}

/// States what a pooled ordering was derived from, and only when that is
/// observed usage — the same disclosure the router puts on its `reason`, so the
/// two never disagree. A quota-derived order needs no caveat.
fn pool_basis_suffix(basis: RoomBasis) -> String {
    match basis {
        RoomBasis::ObservedUsage => format!(" [pool order: {}]", RoomBasis::ObservedUsage.label()),
        RoomBasis::Quota => String::new(),
    }
}

/// Per-target windowed usage + headroom table (`rtrt usage`).
fn run_usage(format: ReportFormatArg) -> Result<()> {
    let config = effective_config_for_cwd();
    let windows = provider_usage_windows();
    let headroom = target_headroom(&config);

    match format {
        ReportFormatArg::Json => {
            let value = serde_json::json!({
                "windows": windows,
                "headroom": headroom,
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        ReportFormatArg::Table => print_usage_table(&windows, &headroom),
    }
    Ok(())
}

fn print_usage_table(
    windows: &BTreeMap<String, TargetWindows>,
    headroom: &BTreeMap<String, TargetHeadroom>,
) {
    let mut targets = windows.keys().cloned().collect::<Vec<_>>();
    for target in headroom.keys() {
        targets.push(target.clone());
    }
    targets.sort();
    targets.dedup();

    if targets.is_empty() {
        println!("no provider usage recorded yet (~/.rtrt/provider-usage.tsv is empty)");
        return;
    }

    println!(
        "provider usage (rolling windows; ~ = estimated, CLI shell-outs report no real tokens)"
    );
    println!(
        "{:<16} {:>14} {:>14} {:>14}  headroom (24h vs daily limit)",
        "target", "5h tok/req", "24h tok/req", "7d tok/req"
    );
    for target in targets {
        let window = windows.get(&target).copied().unwrap_or_default();
        let head = headroom
            .get(&target)
            .map(format_headroom)
            .unwrap_or_else(|| "?".to_string());
        println!(
            "{:<16} {:>14} {:>14} {:>14}  {}",
            target,
            format_window_cell(&window.last_5h),
            format_window_cell(&window.last_24h),
            format_window_cell(&window.last_7d),
            head,
        );
    }
}

fn format_window_cell(usage: &rtrt_providers::WindowUsage) -> String {
    format!(
        "{}/{}",
        format_token_count(usage.tokens, usage.has_estimates()),
        usage.requests
    )
}

/// Tokens with a leading `~` when any contributing row was estimated.
fn format_token_count(tokens: u64, estimated: bool) -> String {
    if estimated {
        format!("~{tokens}")
    } else {
        tokens.to_string()
    }
}

/// Remaining headroom against the configured daily limit. `?` when no
/// `[limits]` cap is set — we never fabricate a ceiling.
fn format_headroom(headroom: &TargetHeadroom) -> String {
    if headroom.limits_unknown() {
        return "? (no [limits] cap)".to_string();
    }
    let mut parts = Vec::new();
    if let (Some(remaining), Some(limit)) = (headroom.remaining_tokens, headroom.limit_tokens) {
        let used = format_token_count(headroom.used_tokens, headroom.tokens_estimated);
        parts.push(format!("{remaining}/{limit} tok left (used {used})"));
    }
    if let (Some(remaining), Some(limit)) = (headroom.remaining_requests, headroom.request_limit) {
        parts.push(format!(
            "{remaining}/{limit} req left (used {})",
            headroom.used_requests
        ));
    }
    if parts.is_empty() {
        "?".to_string()
    } else {
        parts.join(", ")
    }
}

async fn run_provider(cmd: ProviderCmd) -> Result<()> {
    let ProviderCmd::Chat {
        prompt,
        model,
        provider,
        stream,
        base_url,
        max_tokens,
        system,
    } = cmd;
    let text = match prompt.as_deref() {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf.trim().to_string()
        }
        Some(s) => s.to_string(),
    };
    if text.is_empty() {
        bail!("prompt is empty");
    }
    let kind = provider.unwrap_or_else(|| detect_provider(&model));
    let mut messages = Vec::new();
    if let Some(sys) = system {
        messages.push(ChatMessage {
            role: Role::System,
            content: sys,
        });
    }
    messages.push(ChatMessage {
        role: Role::User,
        content: text,
    });
    let req = ChatRequest {
        model: model.clone(),
        messages,
        max_tokens,
        temperature: None,
    };

    let provider: Box<dyn Provider> = match kind {
        ProviderArg::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
            Box::new(AnthropicProvider::new(key))
        }
        ProviderArg::Openai => {
            let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
            Box::new(OpenAIProvider::new(key))
        }
        ProviderArg::OpenaiCompat => {
            let url =
                base_url.ok_or_else(|| anyhow::anyhow!("--base-url required for openai-compat"))?;
            let mut p = OpenAICompatibleProvider::new("openai-compat", url);
            if let Ok(key) = std::env::var("RTRT_PROVIDER_API_KEY") {
                p = p.with_api_key(key);
            }
            Box::new(p)
        }
    };

    let ledger_target = provider_arg_target(kind);
    if stream {
        let mut s = provider.chat_stream(req).await?;
        let mut stdout = std::io::stdout().lock();
        let mut final_usage = rtrt_providers::Usage::default();
        while let Some(event) = s.next().await {
            match event? {
                ChatStreamEvent::Delta { text } => {
                    write!(stdout, "{text}")?;
                    stdout.flush()?;
                }
                ChatStreamEvent::Usage(u) => final_usage.merge(&u),
                ChatStreamEvent::Done => break,
            }
        }
        writeln!(stdout)?;
        eprintln!(
            "[usage] input={} output={}",
            final_usage.input_tokens, final_usage.output_tokens
        );
        // Real API usage from the stream → record as exact (est = 0).
        record_invocation(
            ledger_target,
            &model,
            final_usage.input_tokens,
            final_usage.output_tokens,
            false,
            true,
        );
    } else {
        let resp = provider.chat(req).await?;
        println!("{}", resp.content);
        eprintln!(
            "[usage] provider={} model={} input={} output={}",
            resp.provider, resp.model, resp.usage.input_tokens, resp.usage.output_tokens
        );
        record_invocation(
            ledger_target,
            &resp.model,
            resp.usage.input_tokens,
            resp.usage.output_tokens,
            false,
            true,
        );
    }
    Ok(())
}

/// Ledger target name for a `provider chat` invocation.
fn provider_arg_target(kind: ProviderArg) -> &'static str {
    match kind {
        ProviderArg::Anthropic => "anthropic",
        ProviderArg::Openai => "openai",
        ProviderArg::OpenaiCompat => "openai-compat",
    }
}

fn run_context(cmd: ContextCmd) -> Result<()> {
    match cmd {
        ContextCmd::Status { repo } => {
            let out = git_capture(&repo, &["status", "--short", "--branch"])?;
            print_filtered(&out, "git status");
        }
        ContextCmd::Diff { base, repo } => {
            let mut args = vec!["diff"];
            if let Some(b) = base.as_deref() {
                args.push(b);
            }
            let out = git_capture(&repo, &args)?;
            print!("{out}");
        }
        ContextCmd::Log { count, repo } => {
            let n = count.to_string();
            let out = git_capture(&repo, &["log", "--oneline", "-n", &n])?;
            print_filtered(&out, "git log");
        }
    }
    Ok(())
}

fn git_capture(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&out.stdout));
    if !out.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(combined)
}

fn print_filtered(raw: &str, command: &str) {
    let out = match rtrt_proxy::filter_for(command) {
        Some(f) => f.apply(raw),
        None => raw.to_string(),
    };
    print!("{out}");
}

fn run_prompt(cmd: PromptCmd) -> Result<()> {
    match cmd {
        PromptCmd::Save {
            name,
            body,
            meta,
            registry,
        } => {
            let reg = PromptRegistry::open(&registry)?;
            let body = read_body_or_stdin(body)?;
            let mut metadata = std::collections::BTreeMap::new();
            for (k, v) in meta {
                metadata.insert(k, v);
            }
            let saved = reg.save(&name, &body, metadata)?;
            println!(
                "saved {} v{} ({} bytes)",
                saved.name,
                saved.version,
                saved.body.len()
            );
        }
        PromptCmd::Get {
            name,
            version,
            registry,
        } => {
            let reg = PromptRegistry::open(&registry)?;
            let prompt = match version {
                Some(v) => reg.get(&name, v)?,
                None => reg
                    .latest(&name)?
                    .ok_or_else(|| anyhow::anyhow!("no versions saved for {name}"))?,
            };
            println!("{}", prompt.body);
            eprintln!(
                "[prompt] {} v{} created_at={} parent={:?}",
                prompt.name, prompt.version, prompt.created_at, prompt.parent_version
            );
        }
        PromptCmd::List { registry } => {
            let reg = PromptRegistry::open(&registry)?;
            let names = reg.list_names()?;
            if names.is_empty() {
                println!("(no prompts saved)");
            } else {
                for name in names {
                    let versions = reg.list_versions(&name)?;
                    println!("{} ({} version(s))", name, versions.len());
                }
            }
        }
        PromptCmd::Versions { name, registry } => {
            let reg = PromptRegistry::open(&registry)?;
            for v in reg.list_versions(&name)? {
                let p = reg.get(&name, v)?;
                println!(
                    "v{:>3}  parent={:?}  {} bytes",
                    p.version,
                    p.parent_version,
                    p.body.len()
                );
            }
        }
    }
    Ok(())
}

fn current_project_identity() -> Result<ProjectIdentity> {
    let cwd = std::env::current_dir().context("resolve actual current directory")?;
    ProjectIdentity::derive(&cwd).map_err(anyhow::Error::from)
}

fn project_claim_matches(identity: &ProjectIdentity, claim: &str) -> bool {
    let claim = claim.trim();
    claim.is_empty()
        || claim == identity.slug()
        || claim == identity.label()
        || claim == identity.fingerprint()
        || Path::new(claim)
            .canonicalize()
            .is_ok_and(|path| path == identity.memory_root())
}

/// Human project values are assertions only. None can select a store or row
/// namespace; every accepted normal operation uses the canonical identity slug.
fn assert_current_project(identity: &ProjectIdentity, explicit: Option<&str>) -> Result<String> {
    for (source, claim) in [
        ("--project", explicit.map(str::to_string)),
        ("RTRT_PROJECT", nonempty_env("RTRT_PROJECT")),
        ("RTRT_DEFAULT_PROJECT", nonempty_env("RTRT_DEFAULT_PROJECT")),
        ("RTRT_PARENT_PROJECT", nonempty_env("RTRT_PARENT_PROJECT")),
    ] {
        if let Some(claim) = claim
            && !project_claim_matches(identity, &claim)
        {
            bail!(
                "foreign project assertion rejected: {source}={claim:?}; current project is {} ({})",
                identity.label(),
                identity.slug()
            );
        }
    }
    Ok(identity.slug().to_string())
}

fn resolve_hook_project(explicit: Option<String>) -> Result<(ProjectIdentity, String)> {
    let identity = current_project_identity()?;
    let project = assert_current_project(&identity, explicit.as_deref())?;
    Ok((identity, project))
}

/// Parse an env var into `T`, falling back to `default` when unset or
/// unparseable. Used by the hook commands to layer env over config.
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// SessionEnd compression sweep. Mirrors the dashboard's auto-compress
/// daemon but as a one-shot CLI pass so users without a long-lived
/// dashboard still get automatic compression. No-op unless
/// `RTRT_AUTO_COMPRESS_LLM=1`; honours the same `RTRT_AUTO_COMPRESS_*`
/// knobs as the daemon.
async fn run_hook_compress(project: Option<String>, store: Option<PathBuf>) -> Result<()> {
    // Resolution order for every knob: env var > ~/.rtrt/config.toml >
    // built-in default. The config file lets users keep their local model
    // choice out of ~/.claude/settings.json.
    let cfg = rtrt_core::Config::load().unwrap_or_default().auto_compress;
    let enabled = match std::env::var("RTRT_AUTO_COMPRESS_LLM") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
        Err(_) => cfg.enabled,
    };
    if !enabled {
        return Ok(());
    }
    let (identity, project) = resolve_hook_project(project)?;
    reject_normal_store_override(store.as_deref())?;
    let store_path = rtrt_core::project_memory_db_path(&identity)?;
    if !store_path.exists() {
        return Ok(());
    }
    let age_sec: i64 = env_or("RTRT_AUTO_COMPRESS_AGE_SEC", cfg.age_sec);
    let min_chars: usize = env_or("RTRT_AUTO_COMPRESS_MIN_CHARS", cfg.min_chars);
    let batch: usize = env_or("RTRT_AUTO_COMPRESS_BATCH", cfg.batch);
    let model = std::env::var("RTRT_AUTO_COMPRESS_MODEL").unwrap_or_else(|_| cfg.model.clone());
    let max_tokens: u32 = env_or("RTRT_AUTO_COMPRESS_MAX_TOKENS", cfg.max_tokens);
    let memory = MemoryStore::open_project(&identity)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let candidates = memory.compress_candidates(&project, now - age_sec, min_chars, batch)?;
    if candidates.is_empty() {
        return Ok(());
    }
    let gateway = gateway_from_env_or_config(cfg.base_url.as_deref());
    let mut compressed = 0usize;
    for (id, body) in candidates {
        let req = ChatRequest {
            model: model.clone(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are a lossless-meaning compressor. Rewrite the user message in the shortest form that preserves every fact, decision, file path, identifier, command, and number. Drop filler, hedging, headings, and greetings. Plain text only. No commentary.".to_string(),
                },
                ChatMessage {
                    role: Role::User,
                    content: body.clone(),
                },
            ],
            max_tokens: Some(max_tokens),
            temperature: Some(0.0),
        };
        let Ok(resp) = gateway.chat(req).await else {
            continue;
        };
        let new_body = resp.content.trim().to_string();
        let mut meta = memory.get_metadata(id).unwrap_or_default();
        meta.insert("compressed_at".into(), now.to_string());
        if new_body.is_empty() || new_body.len() >= body.len() {
            meta.insert("compressed_skip".into(), "no-shrink".into());
            let _ = memory.set_metadata(id, &meta);
            continue;
        }
        if memory.compress_in_place(id, &new_body).is_err() {
            continue;
        }
        meta.insert("compressed_model".into(), model.clone());
        meta.insert("compressed_from_chars".into(), body.len().to_string());
        meta.insert("compressed_to_chars".into(), new_body.len().to_string());
        let _ = memory.set_metadata(id, &meta);
        compressed += 1;
    }
    if compressed > 0 {
        eprintln!("rtrt hook compress: {compressed} rows compressed in {project}");
    }
    Ok(())
}

fn run_hook_capture(cmd: HookCmd) -> Result<()> {
    match cmd {
        HookCmd::Recall { .. }
        | HookCmd::Compress { .. }
        | HookCmd::SessionInject { .. }
        | HookCmd::Provenance { .. }
        | HookCmd::Style
        | HookCmd::StyleInject
        | HookCmd::Statusline
        | HookCmd::ProxyRewrite => {}
        HookCmd::Capture {
            kind,
            project,
            store,
        } => {
            let mut raw = String::new();
            std::io::stdin().read_to_string(&mut raw).ok();
            if raw.trim().is_empty() {
                return Ok(());
            }
            // Extract a human-readable summary from the Claude Code hook
            // payload (JSON). Falls back to the raw text when the payload
            // isn't JSON. Returns None for low-signal events we choose to
            // skip (e.g. an empty prompt or a tool with no useful input).
            let Some(summary) = summarize_hook_payload(&kind, &raw) else {
                return Ok(());
            };
            // Strip control bytes and clip to 4 KB.
            let cleaned: String = summary
                .chars()
                .filter(|c| !c.is_control() || matches!(*c, '\n' | '\r' | '\t'))
                .take(4096)
                .collect();
            if cleaned.trim().is_empty() {
                return Ok(());
            }
            let redacted = rtrt_compress::redact_secrets(&cleaned);
            let (identity, project) = resolve_hook_project(project)?;
            reject_normal_store_override(store.as_deref())?;
            let memory = MemoryStore::open_project(&identity)
                .context("open current project memory store")?;
            // Dedup: skip if an identical body landed in this project within
            // the window. Kills the repeated near-identical PostToolBatch /
            // PostToolUse rows a busy session produces.
            let window: i64 = std::env::var("RTRT_AUTO_DEDUP_WINDOW_SEC")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300);
            let sha = MemoryStore::body_sha(&redacted);
            if window > 0 {
                if let Ok(Some(seen_at)) = memory.body_seen_at(&project, &sha) {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    if now.saturating_sub(seen_at) < window {
                        return Ok(());
                    }
                }
            }
            let mut meta: BTreeMap<String, String> = BTreeMap::new();
            meta.insert("source".into(), "claude-code".into());
            let id = memory.save_with_metadata(&project, &kind, &redacted, &meta)?;
            let session = std::env::var("RTRT_SESSION_ID")
                .ok()
                .or_else(|| extract_json_str(&raw, "session_id"));
            let _ = memory.tag_row(id, session.as_deref(), Some(&sha));
        }
    }
    Ok(())
}

fn run_hook_provenance(store: Option<PathBuf>) -> Result<()> {
    let Some(invocation_id) = nonempty_env("RTRT_INVOCATION_ID") else {
        return Ok(());
    };
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok();
    let child_session_id =
        extract_json_str(&raw, "session_id").or_else(|| nonempty_env("RTRT_CHILD_SESSION_ID"));
    let Some(child_session_id) = child_session_id.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };

    let parent_cwd = nonempty_env("RTRT_PARENT_CWD");
    let parent_worktree = nonempty_env("RTRT_PARENT_WORKTREE");
    let parent_project = parent_worktree
        .as_deref()
        .or(parent_cwd.as_deref())
        .map(rtrt_core::project_for_cwd_str)
        .or_else(|| nonempty_env("RTRT_PARENT_PROJECT"));
    let Some(parent_project) = parent_project.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let identity = current_project_identity()?;
    assert_current_project(&identity, Some(&parent_project))?;
    reject_normal_store_override(store.as_deref())?;
    let memory =
        MemoryStore::open_project(&identity).context("open current project memory store")?;
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    memory.upsert_invocation_provenance(&InvocationProvenance {
        child_session_id,
        invocation_id,
        parent_project: identity.slug().to_string(),
        parent_session_id: nonempty_env("RTRT_PARENT_SESSION_ID"),
        parent_call_id: nonempty_env("RTRT_PARENT_CALL_ID"),
        caller_agent: nonempty_env("RTRT_PARENT_AGENT"),
        parent_cwd,
        parent_worktree,
        target: nonempty_env("RTRT_CHILD_TARGET"),
        model: nonempty_env("RTRT_CHILD_MODEL"),
        created_at,
    })?;
    Ok(())
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn run_hook_proxy_rewrite() -> Result<()> {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return Ok(());
    }
    if line.trim().is_empty() {
        return Ok(());
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&line) else {
        return Ok(());
    };
    if payload.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return Ok(());
    }
    let Some(command) = payload
        .get("tool_input")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())
    else {
        return Ok(());
    };
    let trimmed = command.trim_start();
    if trimmed.starts_with(PROXY_RUN_PREFIX) || trimmed.starts_with(LEGACY_PROXY_PREFIX) {
        return Ok(());
    }
    if SHELL_COMPLEX_MARKERS
        .iter()
        .any(|marker| command.contains(marker))
    {
        return Ok(());
    }
    let Some(token) = first_whitespace_token(trimmed) else {
        return Ok(());
    };
    let optimizable =
        rtrt_proxy::filter_for(token).is_some() || KNOWN_SHRINKABLE_COMMANDS.contains(&token);
    if !optimizable {
        return Ok(());
    }
    let updated = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": {
                "command": format!("{PROXY_RUN_PREFIX} {command}")
            }
        }
    });
    if let Ok(rendered) = serde_json::to_string(&updated) {
        println!("{rendered}");
    }
    Ok(())
}

/// Walk up from `start` to the enclosing repo root (first ancestor with a
/// `.git` or `.rtrt`), falling back to `start` itself. Used to resolve which
/// `<repo>/.rtrt/config.toml` a hook or status line should read its
/// per-project customization from.
fn repo_root_from_cwd(start: &std::path::Path) -> PathBuf {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() || dir.join(".rtrt").exists() {
            return dir.to_path_buf();
        }
        cur = dir.parent();
    }
    start.to_path_buf()
}

/// Resolve the repo root from a hook payload's `cwd` field, if present.
fn hook_repo_root(raw: &str) -> Option<PathBuf> {
    extract_json_str(raw, "cwd").map(|cwd| repo_root_from_cwd(std::path::Path::new(&cwd)))
}

/// The repo root enclosing the current working directory, if any. Returns
/// `None` when the cwd is not inside a repo (no `.git` / `.rtrt` ancestor) so
/// callers fall back to the global config — keeping the no-repo path identical
/// to today's behavior.
fn cwd_repo_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let root = repo_root_from_cwd(&cwd);
    (root.join(".git").exists() || root.join(".rtrt").exists()).then_some(root)
}

/// Load the config effective for the current working directory: the global
/// config overlaid with `<repo>/.rtrt/config.toml` when the cwd is inside a
/// repo, else the plain global config. Errors fall back to the default config
/// so a malformed per-project file never breaks a routing/compression command.
pub(crate) fn effective_config_for_cwd() -> rtrt_core::Config {
    rtrt_core::Config::load_effective(cwd_repo_root().as_deref()).unwrap_or_default()
}

/// Refuse to act on a target the repo's effective config explicitly disables
/// (`[agents] <name> = false` / `[providers] <name> = false`). Only an explicit
/// `false` blocks; an absent entry leaves global behavior unchanged.
fn ensure_target_enabled(target: &str) -> Result<()> {
    let cfg = effective_config_for_cwd();
    let disabled = cfg.providers.enabled_override(target) == Some(false)
        || cfg.agents.enabled_override(target) == Some(false);
    if disabled {
        bail!("target '{target}' is disabled for this project (.rtrt/config.toml)");
    }
    Ok(())
}

fn run_hook_style() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok();
    let repo = hook_repo_root(&raw);
    let prompt = extract_json_str(&raw, "prompt").unwrap_or_else(|| raw.trim().to_string());
    if let Some(level) = parse_output_switch(&prompt) {
        rtrt_core::write_output_style_level_for(repo.as_deref(), level)?;
        let scope = if repo.is_some() {
            "this project"
        } else {
            "globally"
        };
        let reason = if level.is_active() {
            format!(
                "Output Optimizer terse mode set to {} for {scope}.",
                level.as_str()
            )
        } else {
            format!("Output Optimizer terse mode off for {scope}.")
        };
        print_hook_block(&reason);
        return Ok(());
    }

    let level = rtrt_core::read_output_style_level_for(repo.as_deref());
    if level.is_active() {
        print_hook_context(&style_reinforcement(level));
    }
    Ok(())
}

fn run_hook_style_inject() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok();
    let repo = hook_repo_root(&raw);
    let level = rtrt_core::read_output_style_level_for(repo.as_deref());
    if level.is_active() {
        println!("{}", style_session_block(level));
    }
    Ok(())
}

fn parse_output_switch(prompt: &str) -> Option<OutputStyleLevel> {
    let mut parts = prompt.split_whitespace();
    let cmd = parts.next()?;
    if cmd != "/output" {
        return None;
    }
    let level = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    OutputStyleLevel::parse(level)
}

fn print_hook_context(context: &str) {
    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context
        }
    });
    println!("{payload}");
}

fn print_hook_block(reason: &str) {
    let payload = serde_json::json!({
        "decision": "block",
        "reason": reason,
    });
    println!("{payload}");
}

fn style_reinforcement(level: OutputStyleLevel) -> String {
    setup::style_reinforcement(level)
}

fn style_session_block(level: OutputStyleLevel) -> String {
    setup::style_session_block(level)
}

struct StatuslineOptions {
    rich: bool,
    format: Option<String>,
}

struct OpenCodeStatuslineOptions {
    cwd: Option<PathBuf>,
    session: Option<String>,
    model: Option<String>,
    width: usize,
    budget_ms: u64,
    no_git: bool,
    refresh: bool,
}

struct OpenCodeBudget {
    started: std::time::Instant,
    limit: std::time::Duration,
}

impl OpenCodeBudget {
    fn new(started: std::time::Instant, budget_ms: u64) -> Self {
        Self {
            started,
            limit: std::time::Duration::from_millis(budget_ms),
        }
    }

    fn expired(&self) -> bool {
        self.started.elapsed() >= self.limit
    }

    fn remaining(&self) -> std::time::Duration {
        self.limit.saturating_sub(self.started.elapsed())
    }
}

#[derive(Debug, Clone)]
struct OpenCodeSavings {
    sigma_pct: u64,
    command_pct: Option<u64>,
    saved_chars: Option<u64>,
    base_chars: Option<u64>,
    recall_chars: Option<u64>,
    cached: bool,
}

#[derive(Debug, Clone)]
struct OpenCodeHeadroom {
    text: String,
    tone: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
struct ClaudeRateLimitWindow {
    used_percentage: f64,
    resets_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct ClaudeRateLimits {
    captured_at: u64,
    five_hour: Option<ClaudeRateLimitWindow>,
    seven_day: Option<ClaudeRateLimitWindow>,
}

#[derive(Debug, Clone)]
struct OpenCodeGitSnapshot {
    branch: String,
    dirty: bool,
    cached: bool,
}

#[derive(Debug, Clone)]
struct OpenCodeMemoryAggregate {
    rows: u64,
    original_chars: u64,
    stored_chars: u64,
}

struct OpenCodeRenderFacts {
    rendered_at: u64,
    project: String,
    style: OutputStyleLevel,
    savings: Option<OpenCodeSavings>,
    headroom: Option<OpenCodeHeadroom>,
    quota: Option<ClaudeRateLimits>,
    git: Option<OpenCodeGitSnapshot>,
    session: Option<String>,
    model: Option<String>,
    memory: Option<OpenCodeMemoryAggregate>,
}

struct OpenCodeSegment {
    id: &'static str,
    text: String,
    tone: &'static str,
    pri: u8,
}

impl OpenCodeSegment {
    fn json(self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "text": self.text,
            "tone": self.tone,
            "pri": self.pri,
        })
    }
}

const OPENCODE_STATUSLINE_VERSION: u64 = 1;
const OPENCODE_GIT_CACHE_TTL_SECS: u64 = 5;
const OPENCODE_GIT_OUTPUT_CAP: u64 = 64 * 1024;
const CLAUDE_RATE_LIMIT_CACHE_VERSION: u64 = 1;
const CLAUDE_RATE_LIMIT_CACHE_MAX_BYTES: u64 = 4 * 1024;
const CLAUDE_RATE_LIMIT_DEFAULT_MAX_AGE_SECS: u64 = 15 * 60;
const CLAUDE_RATE_LIMIT_MAX_MAX_AGE_SECS: u64 = 24 * 60 * 60;
const CLAUDE_RATE_LIMIT_SOURCE: &str = "claude_statusline";

#[derive(Clone, Copy)]
enum RateLimitCacheParentPolicy {
    Private,
    OwnerNonWritable,
}

fn print_opencode_statusline(opts: OpenCodeStatuslineOptions) {
    let started = std::time::Instant::now();
    let budget = OpenCodeBudget::new(started, opts.budget_ms);
    let ts = epoch_secs();
    let mut degraded = Vec::new();
    let mut stale = false;

    let (cwd, canonical) = canonical_statusline_cwd(opts.cwd);
    if !canonical {
        add_degraded(&mut degraded, "cwd");
    }
    let cwd_text = sanitize_statusline_text(&cwd.to_string_lossy(), 4096);
    let repo_root = repo_root_from_cwd(&cwd);
    let project = sanitize_statusline_text(&rtrt_core::project_for_cwd(&cwd), 160);
    let style = rtrt_core::read_output_style_level_for(Some(&repo_root));
    let session = opts
        .session
        .as_deref()
        .map(|value| sanitize_statusline_text(value, 160))
        .filter(|value| !value.is_empty());
    let model = opts
        .model
        .as_deref()
        .map(|value| sanitize_statusline_text(value, 160))
        .filter(|value| !value.is_empty());

    let mut data = serde_json::Map::new();
    data.insert("style".into(), serde_json::json!(style.as_str()));
    data.insert("width".into(), serde_json::json!(opts.width));
    if let Some(session) = &session {
        data.insert("session".into(), serde_json::json!(session));
    }
    if let Some(model) = &model {
        data.insert("model".into(), serde_json::json!(model));
    }

    let quota = if budget.expired() {
        add_degraded(&mut degraded, "quota");
        None
    } else {
        let value = collect_opencode_quota(ts, &budget);
        if let Some(value) = &value {
            data.insert("quota".into(), opencode_quota_json(value, ts));
        } else {
            add_degraded(&mut degraded, "quota");
        }
        value
    };

    let savings = if budget.expired() {
        add_degraded(&mut degraded, "savings");
        None
    } else {
        let value = collect_opencode_savings(&project, opts.refresh, &budget);
        if let Some(value) = &value {
            data.insert("savings".into(), opencode_savings_json(value));
        } else {
            add_degraded(&mut degraded, "savings");
        }
        value
    };

    let git = if opts.width >= 100 && !opts.no_git {
        let value = collect_opencode_git(&cwd, &budget, opts.refresh);
        match value {
            Some((snapshot, git_stale)) => {
                stale |= git_stale;
                if git_stale {
                    add_degraded(&mut degraded, "git_stale");
                }
                data.insert(
                    "git".into(),
                    serde_json::json!({
                        "branch": snapshot.branch.clone(),
                        "dirty": snapshot.dirty,
                        "cached": snapshot.cached,
                    }),
                );
                Some(snapshot)
            }
            None => {
                add_degraded(&mut degraded, "git");
                None
            }
        }
    } else {
        None
    };

    let headroom = if opts.width >= 60 {
        if budget.expired() {
            add_degraded(&mut degraded, "headroom");
            None
        } else {
            match collect_opencode_headroom(&repo_root, model.as_deref(), &budget) {
                Some((headroom_data, summary)) => {
                    data.insert("headroom".into(), headroom_data);
                    if summary.is_none() {
                        add_degraded(&mut degraded, "headroom");
                    }
                    summary
                }
                None => {
                    add_degraded(&mut degraded, "headroom");
                    None
                }
            }
        }
    } else {
        None
    };

    let memory = if opts.width >= 100 {
        if budget.expired() {
            add_degraded(&mut degraded, "memory");
            None
        } else {
            let value = collect_opencode_memory(&project, budget.remaining());
            if let Some(value) = &value {
                data.insert("memory".into(), opencode_memory_json(value));
            } else {
                add_degraded(&mut degraded, "memory");
            }
            value
        }
    } else {
        None
    };

    if budget.expired() {
        stale = true;
        add_degraded(&mut degraded, "budget");
    }
    let facts = OpenCodeRenderFacts {
        rendered_at: ts,
        project: project.clone(),
        style,
        savings,
        headroom,
        quota,
        git,
        session,
        model,
        memory,
    };
    let segments = render_opencode_segments(opts.width, &facts);
    let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let payload = serde_json::json!({
        "v": OPENCODE_STATUSLINE_VERSION,
        "ts": ts,
        "took_ms": took_ms,
        "stale": stale,
        "degraded": degraded,
        "project": project,
        "cwd": cwd_text,
        "data": data,
        "segments": segments,
    });
    let line = serde_json::to_string(&payload).unwrap_or_else(|_| {
        "{\"v\":1,\"ts\":0,\"took_ms\":0,\"stale\":true,\"degraded\":[\"json\"],\"project\":\"\",\"cwd\":\"\",\"data\":{},\"segments\":[]}".to_string()
    });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
}

fn render_opencode_segments(width: usize, facts: &OpenCodeRenderFacts) -> Vec<serde_json::Value> {
    let mut segments = Vec::new();
    if width < 60 {
        if let Some(savings) = &facts.savings {
            segments.push(OpenCodeSegment {
                id: "sigma",
                text: format!("Σ:{}%", savings.sigma_pct),
                tone: "good",
                pri: 100,
            });
        }
        segments.push(style_segment(facts.style, 90));
        let mut rendered = segments
            .into_iter()
            .map(OpenCodeSegment::json)
            .collect::<Vec<_>>();
        append_opencode_quota_segments(&mut rendered, facts.quota.as_ref(), facts.rendered_at);
        return rendered;
    }

    segments.push(OpenCodeSegment {
        id: "project",
        text: facts.project.clone(),
        tone: "accent",
        pri: 100,
    });
    segments.push(style_segment(facts.style, 90));
    if let Some(savings) = &facts.savings {
        let command = savings
            .command_pct
            .map(|pct| format!(" cmd:{pct}%"))
            .unwrap_or_default();
        segments.push(OpenCodeSegment {
            id: "savings",
            text: format!("save:{}%{command}", savings.sigma_pct),
            tone: "good",
            pri: 80,
        });
    }
    let mut rendered = segments
        .into_iter()
        .map(OpenCodeSegment::json)
        .collect::<Vec<_>>();
    append_opencode_quota_segments(&mut rendered, facts.quota.as_ref(), facts.rendered_at);
    if let Some(headroom) = &facts.headroom {
        rendered.push(
            OpenCodeSegment {
                id: "headroom",
                text: headroom.text.clone(),
                tone: headroom.tone,
                pri: 70,
            }
            .json(),
        );
    }

    if width >= 100 {
        if let Some(git) = &facts.git {
            rendered.push(
                OpenCodeSegment {
                    id: "git",
                    text: format!("{}{}", git.branch, if git.dirty { "*" } else { "" }),
                    tone: if git.dirty { "warn" } else { "muted" },
                    pri: 65,
                }
                .json(),
            );
        }
        if let Some(model) = &facts.model {
            rendered.push(
                OpenCodeSegment {
                    id: "model",
                    text: model.clone(),
                    tone: "muted",
                    pri: 60,
                }
                .json(),
            );
        }
        if let Some(session) = &facts.session {
            rendered.push(
                OpenCodeSegment {
                    id: "session",
                    text: format!("sess:{}", session.chars().take(12).collect::<String>()),
                    tone: "muted",
                    pri: 50,
                }
                .json(),
            );
        }
        if let Some(memory) = &facts.memory {
            let pct = (memory.original_chars > 0).then(|| {
                savings_pct(
                    memory.original_chars.saturating_sub(memory.stored_chars),
                    memory.original_chars,
                )
            });
            rendered.push(
                OpenCodeSegment {
                    id: "memory",
                    text: pct.map_or_else(
                        || format!("mem:{}", memory.rows),
                        |pct| format!("mem:{} {pct}%", memory.rows),
                    ),
                    tone: if pct.is_some_and(|pct| pct > 0) {
                        "good"
                    } else {
                        "muted"
                    },
                    pri: 40,
                }
                .json(),
            );
        }
    }

    rendered
}

fn style_segment(style: OutputStyleLevel, pri: u8) -> OpenCodeSegment {
    OpenCodeSegment {
        id: "style",
        text: format!("opt:{}", style.as_str()),
        tone: if style.is_active() { "accent" } else { "muted" },
        pri,
    }
}

fn append_opencode_quota_segments(
    segments: &mut Vec<serde_json::Value>,
    quota: Option<&ClaudeRateLimits>,
    now: u64,
) {
    let Some(quota) = quota else {
        return;
    };
    let freshness_sec = now.saturating_sub(quota.captured_at);
    if let Some(window) = &quota.five_hour {
        segments.push(opencode_quota_segment(
            "limit_5h",
            "5h",
            window,
            now,
            freshness_sec,
            75,
        ));
    }
    if let Some(window) = &quota.seven_day {
        segments.push(opencode_quota_segment(
            "limit_week",
            "wk",
            window,
            now,
            freshness_sec,
            74,
        ));
    }
}

fn opencode_quota_segment(
    id: &'static str,
    label: &'static str,
    window: &ClaudeRateLimitWindow,
    now: u64,
    freshness_sec: u64,
    pri: u8,
) -> serde_json::Value {
    let remaining = window.resets_at.saturating_sub(now).min(i64::MAX as u64) as i64;
    let rounded_percentage = window.used_percentage.round() as u64;
    serde_json::json!({
        "id": id,
        "text": format!(
            "{label}:{}% ↻{}",
            rounded_percentage,
            humanize_remaining(remaining)
        ),
        "tone": rate_limit_tone(rounded_percentage),
        "pri": pri,
        "source": CLAUDE_RATE_LIMIT_SOURCE,
        "freshness_sec": freshness_sec,
    })
}

fn rate_limit_tone(used_percentage: u64) -> &'static str {
    if used_percentage >= 90 {
        "bad"
    } else if used_percentage >= 70 {
        "warn"
    } else {
        "good"
    }
}

fn canonical_statusline_cwd(requested: Option<PathBuf>) -> (PathBuf, bool) {
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let requested = requested.unwrap_or_else(|| current.clone());
    let absolute = if requested.is_absolute() {
        requested
    } else {
        current.join(requested)
    };
    match std::fs::canonicalize(&absolute) {
        Ok(path) => (path, true),
        Err(_) => (normalize_statusline_path(&absolute), false),
    }
}

fn normalize_statusline_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn sanitize_statusline_text(value: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max_chars));
    for (count, ch) in value.chars().enumerate() {
        if count >= max_chars {
            break;
        }
        if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn add_degraded(degraded: &mut Vec<String>, id: &str) {
    if !degraded.iter().any(|item| item == id) {
        degraded.push(id.to_string());
    }
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn claude_rate_limit_cache_override() -> Option<PathBuf> {
    std::env::var_os("RTRT_CLAUDE_RATE_LIMIT_CACHE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Resolve current and legacy caches beneath a safe canonical `~/.rtrt`.
/// Existing `0755` state directories are safe to traverse; the dedicated
/// `statusline` child is still created and validated as exact `0700`.
fn default_claude_rate_limit_cache_paths(create_state: bool) -> Option<(PathBuf, PathBuf)> {
    let home = std::fs::canonicalize(home_dir()?).ok()?;
    let home_metadata = std::fs::symlink_metadata(&home).ok()?;
    if !home_metadata.is_dir() || home_metadata.file_type().is_symlink() {
        return None;
    }
    let state = home.join(".rtrt");
    match std::fs::symlink_metadata(&state) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_state => {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&state) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return None,
            }
        }
        Err(_) => return None,
    }
    let state_metadata = std::fs::symlink_metadata(&state).ok()?;
    if !state_metadata.is_dir() || state_metadata.file_type().is_symlink() {
        return None;
    }
    let canonical_state = std::fs::canonicalize(&state).ok()?;
    let canonical_metadata = std::fs::symlink_metadata(&canonical_state).ok()?;
    if !canonical_metadata.is_dir()
        || canonical_metadata.file_type().is_symlink()
        || canonical_state.parent() != Some(home.as_path())
    {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if state_metadata.dev() != canonical_metadata.dev()
            || state_metadata.ino() != canonical_metadata.ino()
            || canonical_metadata.uid() != home_metadata.uid()
            || canonical_metadata.permissions().mode() & 0o022 != 0
        {
            return None;
        }
    }
    #[cfg(not(unix))]
    let _ = home_metadata;

    Some((
        canonical_state
            .join("statusline")
            .join("claude-rate-limits.json"),
        canonical_state.join("claude-rate-limits.json"),
    ))
}

fn claude_rate_limit_max_age_secs() -> u64 {
    bounded_claude_rate_limit_max_age(
        std::env::var("RTRT_CLAUDE_RATE_LIMIT_MAX_AGE_SEC")
            .ok()
            .as_deref(),
    )
}

fn bounded_claude_rate_limit_max_age(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=CLAUDE_RATE_LIMIT_MAX_MAX_AGE_SECS).contains(value))
        .unwrap_or(CLAUDE_RATE_LIMIT_DEFAULT_MAX_AGE_SECS)
}

fn collect_opencode_quota(now: u64, budget: &OpenCodeBudget) -> Option<ClaudeRateLimits> {
    if budget.expired() {
        return None;
    }
    let max_age_secs = claude_rate_limit_max_age_secs();
    if let Some(path) = claude_rate_limit_cache_override() {
        return read_claude_rate_limit_cache(
            &path,
            RateLimitCacheParentPolicy::Private,
            now,
            max_age_secs,
            budget,
        );
    }
    let (path, legacy) = default_claude_rate_limit_cache_paths(false)?;
    if let Some(quota) = read_claude_rate_limit_cache(
        &path,
        RateLimitCacheParentPolicy::Private,
        now,
        max_age_secs,
        budget,
    ) {
        return Some(quota);
    }
    let quota = read_claude_rate_limit_cache(
        &legacy,
        RateLimitCacheParentPolicy::OwnerNonWritable,
        now,
        max_age_secs,
        budget,
    )?;
    if !budget.expired() {
        let _ = write_claude_rate_limit_cache(&path, &quota);
    }
    Some(quota)
}

fn read_claude_rate_limit_cache(
    path: &Path,
    parent_policy: RateLimitCacheParentPolicy,
    now: u64,
    max_age_secs: u64,
    budget: &OpenCodeBudget,
) -> Option<ClaudeRateLimits> {
    if budget.expired() {
        return None;
    }
    let raw =
        read_private_rate_limit_cache(path, CLAUDE_RATE_LIMIT_CACHE_MAX_BYTES, parent_policy)?;
    if budget.expired() {
        return None;
    }
    parse_claude_rate_limit_cache(&raw, now, max_age_secs)
}

fn opencode_quota_json(quota: &ClaudeRateLimits, now: u64) -> serde_json::Value {
    let mut windows = serde_json::Map::new();
    if let Some(window) = &quota.five_hour {
        windows.insert("five_hour".into(), rate_limit_window_json(window));
    }
    if let Some(window) = &quota.seven_day {
        windows.insert("seven_day".into(), rate_limit_window_json(window));
    }
    serde_json::json!({
        "source": CLAUDE_RATE_LIMIT_SOURCE,
        "fresh": true,
        "freshness_sec": now.saturating_sub(quota.captured_at),
        "captured_at": quota.captured_at,
        "windows": windows,
    })
}

fn rate_limit_window_json(window: &ClaudeRateLimitWindow) -> serde_json::Value {
    serde_json::json!({
        "used_percentage": window.used_percentage,
        "resets_at": window.resets_at,
    })
}

fn parse_claude_rate_limit_cache(
    raw: &str,
    now: u64,
    max_age_secs: u64,
) -> Option<ClaudeRateLimits> {
    if raw.chars().any(char::is_control) {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let root = value.as_object()?;
    if !has_exact_json_fields(root, &["v", "captured_at", "windows"])
        || root.get("v")?.as_u64()? != CLAUDE_RATE_LIMIT_CACHE_VERSION
    {
        return None;
    }
    let captured_at = root.get("captured_at")?.as_u64()?;
    if captured_at > now || now.saturating_sub(captured_at) > max_age_secs {
        return None;
    }
    let windows = root.get("windows")?.as_object()?;
    if windows.is_empty()
        || windows
            .keys()
            .any(|key| !matches!(key.as_str(), "five_hour" | "seven_day"))
    {
        return None;
    }
    let five_hour = match windows.get("five_hour") {
        Some(value) => Some(parse_cached_rate_limit_window(value)?),
        None => None,
    }
    .filter(|window| window.resets_at > now);
    let seven_day = match windows.get("seven_day") {
        Some(value) => Some(parse_cached_rate_limit_window(value)?),
        None => None,
    }
    .filter(|window| window.resets_at > now);
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    Some(ClaudeRateLimits {
        captured_at,
        five_hour,
        seven_day,
    })
}

fn parse_cached_rate_limit_window(value: &serde_json::Value) -> Option<ClaudeRateLimitWindow> {
    let window = value.as_object()?;
    if !has_exact_json_fields(window, &["used_percentage", "resets_at"]) {
        return None;
    }
    let used_percentage = window.get("used_percentage")?.as_f64()?;
    let resets_at = window.get("resets_at")?.as_u64()?;
    if !used_percentage.is_finite()
        || !(0.0..=100.0).contains(&used_percentage)
        || resets_at > i64::MAX as u64
    {
        return None;
    }
    Some(ClaudeRateLimitWindow {
        used_percentage,
        resets_at,
    })
}

fn has_exact_json_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    expected: &[&str],
) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn read_private_rate_limit_cache(
    path: &Path,
    max_bytes: u64,
    parent_policy: RateLimitCacheParentPolicy,
) -> Option<String> {
    let file_name = path.file_name()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_metadata = std::fs::symlink_metadata(parent).ok()?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return None;
    }
    let canonical_parent = std::fs::canonicalize(parent).ok()?;
    let canonical_parent_metadata = std::fs::symlink_metadata(&canonical_parent).ok()?;
    if canonical_parent_metadata.file_type().is_symlink() || !canonical_parent_metadata.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let parent_mode = canonical_parent_metadata.permissions().mode() & 0o7777;
        let unsafe_mode = match parent_policy {
            RateLimitCacheParentPolicy::Private => parent_mode != 0o700,
            RateLimitCacheParentPolicy::OwnerNonWritable => parent_mode & 0o022 != 0,
        };
        if parent_metadata.dev() != canonical_parent_metadata.dev()
            || parent_metadata.ino() != canonical_parent_metadata.ino()
            || unsafe_mode
        {
            return None;
        }
    }
    #[cfg(not(unix))]
    let _ = parent_policy;

    let target = canonical_parent.join(file_name);
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20_000);
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(&target).ok()?;
    let file_metadata = file.metadata().ok()?;
    let path_metadata = std::fs::symlink_metadata(&target).ok()?;
    if path_metadata.file_type().is_symlink()
        || !file_metadata.is_file()
        || !path_metadata.is_file()
        || file_metadata.len() > max_bytes
    {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if file_metadata.dev() != path_metadata.dev()
            || file_metadata.ino() != path_metadata.ino()
            || file_metadata.uid() != canonical_parent_metadata.uid()
            || file_metadata.nlink() != 1
            || file_metadata.permissions().mode() & 0o7777 != 0o600
        {
            return None;
        }
    }
    let mut raw = String::with_capacity(usize::try_from(file_metadata.len()).ok()?);
    file.take(max_bytes.saturating_add(1))
        .read_to_string(&mut raw)
        .ok()?;
    (raw.len() as u64 <= max_bytes).then_some(raw)
}

fn collect_opencode_savings(
    project: &str,
    refresh: bool,
    budget: &OpenCodeBudget,
) -> Option<OpenCodeSavings> {
    if !refresh
        && let Some(raw) = read_opencode_savings_cache(project)
        && let Some(sigma_pct) = cached_statusline_percentage(&raw, "Σ:")
    {
        return Some(OpenCodeSavings {
            sigma_pct,
            command_pct: cached_statusline_percentage(&raw, "cmd:"),
            saved_chars: None,
            base_chars: None,
            recall_chars: None,
            cached: true,
        });
    }

    let command = read_opencode_proxy_savings(project, budget.remaining());
    if budget.expired() {
        return None;
    }
    let recall = read_opencode_recall_savings(project)?;
    if command.is_none() && recall == 0 {
        return None;
    }
    let (command_saved, command_base) = command.unwrap_or((0, 0));
    let saved = command_saved.saturating_add(recall);
    let base = command_base.saturating_add(recall);
    (base > 0).then(|| OpenCodeSavings {
        sigma_pct: savings_pct(saved, base),
        command_pct: (command_base > 0).then(|| savings_pct(command_saved, command_base)),
        saved_chars: Some(saved),
        base_chars: Some(base),
        recall_chars: Some(recall),
        cached: false,
    })
}

fn read_opencode_savings_cache(project: &str) -> Option<String> {
    let path = savings_cache_path(project);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() > 16 * 1024
        || metadata.modified().ok()?.elapsed().ok()?.as_secs() > SAVINGS_CACHE_TTL_SECS
    {
        return None;
    }
    read_bounded_regular_file(&path, 16 * 1024)
}

fn read_opencode_proxy_savings(project: &str, timeout: std::time::Duration) -> Option<(u64, u64)> {
    let path = proxy_stats::default_path();
    if !is_regular_file(&path) {
        return None;
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(path, flags).ok()?;
    conn.busy_timeout(std::time::Duration::ZERO).ok()?;
    let (saved, input): (i64, i64) = sqlite_query_with_deadline(&conn, timeout, || {
        conn.query_row(
            "SELECT COALESCE(SUM(saved_chars), 0), COALESCE(SUM(input_chars), 0) \
             FROM proxy_runs WHERE saved_chars > 0 AND project = ?1",
            rusqlite::params![project],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    })?;
    (input > 0).then(|| (saved.max(0) as u64, input as u64))
}

fn read_opencode_recall_savings(project: &str) -> Option<u64> {
    let Some(path) = recall_savings_path() else {
        return Some(0);
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(0),
        Err(_) => return None,
    };
    if !metadata.file_type().is_file() || metadata.len() > 4 * 1024 * 1024 {
        return None;
    }
    let raw = read_bounded_regular_file(&path, 4 * 1024 * 1024)?;
    let mut total = 0u64;
    for line in raw.lines() {
        let mut fields = line.split('\t');
        if fields.next() == Some(project)
            && let Some(chars) = fields.next().and_then(|value| value.parse::<u64>().ok())
        {
            total = total.saturating_add(chars);
        }
    }
    Some(total)
}

fn cached_statusline_percentage(raw: &str, marker: &str) -> Option<u64> {
    let rest = raw.split_once(marker)?.1;
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let pct = digits.parse::<u64>().ok()?;
    (pct <= 100).then_some(pct)
}

fn opencode_savings_json(savings: &OpenCodeSavings) -> serde_json::Value {
    let mut value = serde_json::Map::new();
    value.insert("pct".into(), serde_json::json!(savings.sigma_pct));
    value.insert("cached".into(), serde_json::json!(savings.cached));
    if let Some(command_pct) = savings.command_pct {
        value.insert("command_pct".into(), serde_json::json!(command_pct));
    }
    if let Some(saved_chars) = savings.saved_chars {
        value.insert("saved_chars".into(), serde_json::json!(saved_chars));
    }
    if let Some(base_chars) = savings.base_chars {
        value.insert("base_chars".into(), serde_json::json!(base_chars));
    }
    if let Some(recall_chars) = savings.recall_chars {
        value.insert("recall_chars".into(), serde_json::json!(recall_chars));
    }
    serde_json::Value::Object(value)
}

fn collect_opencode_headroom(
    repo_root: &Path,
    model: Option<&str>,
    budget: &OpenCodeBudget,
) -> Option<(serde_json::Value, Option<OpenCodeHeadroom>)> {
    let config = rtrt_core::Config::load_effective(Some(repo_root)).ok()?;
    let mut limits = BTreeMap::new();
    for (target, limit) in &config.limits.targets {
        limits.insert(
            target.trim().to_ascii_lowercase(),
            (limit.daily_tokens, limit.daily_requests),
        );
    }
    let mut usage: BTreeMap<String, (u64, u64, bool)> = BTreeMap::new();
    let ledger_path = std::env::var_os("RTRT_PROVIDER_USAGE_PATH")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".rtrt").join("provider-usage.tsv")));
    if let Some(path) = ledger_path {
        let raw = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= 4 * 1024 * 1024 => {
                read_bounded_regular_file(&path, 4 * 1024 * 1024)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            _ => return None,
        };
        let since = epoch_secs().saturating_sub(24 * 60 * 60);
        for line in raw.lines() {
            if budget.expired() {
                return None;
            }
            let mut fields = line.split('\t');
            let Some(timestamp) = fields
                .next()
                .and_then(|value| value.trim().parse::<u64>().ok())
            else {
                continue;
            };
            let Some(target) = fields.next().map(|value| value.trim().to_ascii_lowercase()) else {
                continue;
            };
            let _model = fields.next();
            let Some(input) = fields
                .next()
                .and_then(|value| value.trim().parse::<u64>().ok())
            else {
                continue;
            };
            let Some(output) = fields
                .next()
                .and_then(|value| value.trim().parse::<u64>().ok())
            else {
                continue;
            };
            let estimated = fields.next().is_some_and(|value| value.trim() != "0");
            if timestamp < since || target.is_empty() {
                continue;
            }
            let entry = usage.entry(target).or_default();
            entry.0 = entry.0.saturating_add(input.saturating_add(output));
            entry.1 = entry.1.saturating_add(1);
            entry.2 |= estimated;
        }
    }
    for target in limits.keys() {
        usage.entry(target.clone()).or_default();
    }
    if usage.is_empty() || budget.expired() {
        return None;
    }

    let mut data = serde_json::Map::new();
    let mut candidates = Vec::new();
    for (target, (used_tokens, used_requests, tokens_estimated)) in usage {
        let target = sanitize_statusline_text(&target, 80);
        let (limit_tokens, request_limit) = limits.get(&target).copied().unwrap_or((None, None));
        let remaining_tokens = limit_tokens.map(|limit| limit.saturating_sub(used_tokens));
        let remaining_requests = request_limit.map(|limit| limit.saturating_sub(used_requests));
        let mut item = serde_json::Map::new();
        item.insert("used_tokens".into(), serde_json::json!(used_tokens));
        item.insert("used_requests".into(), serde_json::json!(used_requests));
        item.insert(
            "tokens_estimated".into(),
            serde_json::json!(tokens_estimated),
        );
        if let Some(limit) = limit_tokens {
            item.insert("limit_tokens".into(), serde_json::json!(limit));
        }
        if let Some(remaining) = remaining_tokens {
            item.insert("remaining_tokens".into(), serde_json::json!(remaining));
        }
        if let Some(limit) = request_limit {
            item.insert("request_limit".into(), serde_json::json!(limit));
        }
        if let Some(remaining) = remaining_requests {
            item.insert("remaining_requests".into(), serde_json::json!(remaining));
        }

        let token_pct = limit_tokens
            .zip(remaining_tokens)
            .filter(|(limit, _)| *limit > 0)
            .map(|(limit, remaining)| percentage_rounded(remaining, limit).min(100));
        let request_pct = request_limit
            .zip(remaining_requests)
            .filter(|(limit, _)| *limit > 0)
            .map(|(limit, remaining)| percentage_rounded(remaining, limit).min(100));
        if let Some(pct) = token_pct.into_iter().chain(request_pct).min() {
            candidates.push((target.clone(), pct, tokens_estimated));
        }
        data.insert(target, serde_json::Value::Object(item));
    }

    let target_hint = model
        .and_then(|model| model.split_once('/').map(|(target, _)| target))
        .map(str::to_ascii_lowercase);
    candidates.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    let selected = target_hint
        .as_deref()
        .and_then(|hint| candidates.iter().find(|item| item.0 == hint))
        .or_else(|| candidates.first());
    let summary = selected.map(|(target, pct, estimated)| OpenCodeHeadroom {
        text: format!("room:{target}:{pct}%"),
        tone: if *estimated {
            "warn"
        } else if *pct <= 10 {
            "bad"
        } else if *pct <= 25 {
            "warn"
        } else {
            "good"
        },
    });
    Some((serde_json::Value::Object(data), summary))
}

fn collect_opencode_memory(
    project: &str,
    timeout: std::time::Duration,
) -> Option<OpenCodeMemoryAggregate> {
    let path = std::env::var_os("RTRT_MEMORY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(rtrt_core::default_memory_store_path);
    if !is_regular_file(&path) {
        return None;
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(path, flags).ok()?;
    conn.busy_timeout(std::time::Duration::ZERO).ok()?;
    let (rows, original_chars, stored_chars): (i64, i64, i64) =
        sqlite_query_with_deadline(&conn, timeout, || {
            conn.query_row(
                "SELECT COUNT(*), \
                        COALESCE(SUM(LENGTH(COALESCE(body_full, body))), 0), \
                        COALESCE(SUM(LENGTH(body)), 0) \
                   FROM memories WHERE project = ?1",
                rusqlite::params![project],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
        })?;
    Some(OpenCodeMemoryAggregate {
        rows: rows.max(0) as u64,
        original_chars: original_chars.max(0) as u64,
        stored_chars: stored_chars.max(0) as u64,
    })
}

fn sqlite_query_with_deadline<T>(
    conn: &rusqlite::Connection,
    timeout: std::time::Duration,
    query: impl FnOnce() -> rusqlite::Result<T>,
) -> Option<T> {
    if timeout.is_zero() {
        return None;
    }
    let interrupt = conn.get_interrupt_handle();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let watchdog = std::thread::Builder::new()
        .name("rtrt-statusline-sqlite".into())
        .spawn(move || {
            if done_rx.recv_timeout(timeout).is_err() {
                interrupt.interrupt();
            }
        })
        .ok()?;
    let result = query().ok();
    let _ = done_tx.send(());
    let _ = watchdog.join();
    result
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return None;
    }
    let mut raw = String::with_capacity(usize::try_from(metadata.len()).ok()?);
    std::fs::File::open(path)
        .ok()?
        .take(max_bytes.saturating_add(1))
        .read_to_string(&mut raw)
        .ok()?;
    (raw.len() as u64 <= max_bytes).then_some(raw)
}

fn opencode_memory_json(memory: &OpenCodeMemoryAggregate) -> serde_json::Value {
    let mut value = serde_json::Map::new();
    value.insert("rows".into(), serde_json::json!(memory.rows));
    value.insert(
        "original_chars".into(),
        serde_json::json!(memory.original_chars),
    );
    value.insert(
        "stored_chars".into(),
        serde_json::json!(memory.stored_chars),
    );
    value.insert(
        "saved_chars".into(),
        serde_json::json!(memory.original_chars.saturating_sub(memory.stored_chars)),
    );
    if memory.original_chars > 0 {
        value.insert(
            "pct".into(),
            serde_json::json!(savings_pct(
                memory.original_chars.saturating_sub(memory.stored_chars),
                memory.original_chars,
            )),
        );
    }
    serde_json::Value::Object(value)
}

fn collect_opencode_git(
    cwd: &Path,
    budget: &OpenCodeBudget,
    refresh: bool,
) -> Option<(OpenCodeGitSnapshot, bool)> {
    let now = epoch_secs();
    let cached = read_opencode_git_cache(cwd);
    if !refresh
        && let Some((snapshot, recorded_at)) = &cached
        && now.saturating_sub(*recorded_at) <= OPENCODE_GIT_CACHE_TTL_SECS
    {
        let mut snapshot = snapshot.clone();
        snapshot.cached = true;
        return Some((snapshot, false));
    }

    let timeout = budget.remaining().min(std::time::Duration::from_millis(50));
    if !timeout.is_zero()
        && let Some(snapshot) = read_bounded_git_snapshot(cwd, timeout)
    {
        if !budget.expired() {
            write_opencode_git_cache(cwd, &snapshot, now);
        }
        return Some((snapshot, false));
    }

    cached.map(|(mut snapshot, recorded_at)| {
        snapshot.cached = true;
        let stale = refresh || now.saturating_sub(recorded_at) > OPENCODE_GIT_CACHE_TTL_SECS;
        (snapshot, stale)
    })
}

fn read_bounded_git_snapshot(
    cwd: &Path,
    timeout: std::time::Duration,
) -> Option<OpenCodeGitSnapshot> {
    let mut child = std::process::Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-C",
        ])
        .arg(cwd)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--no-ahead-behind",
            "--untracked-files=no",
            "--ignore-submodules=all",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let reader = match std::thread::Builder::new()
        .name("rtrt-statusline-git".into())
        .spawn(move || {
            let mut output = Vec::new();
            let _ = stdout
                .take(OPENCODE_GIT_OUTPUT_CAP)
                .read_to_end(&mut output);
            output
        }) {
        Ok(reader) => reader,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().ok()?;
    let status = status?;
    if !status.success() {
        return None;
    }
    let output = String::from_utf8_lossy(&output);
    let branch = output
        .lines()
        .find_map(|line| line.strip_prefix("# branch.head "))?;
    let branch = if branch == "(detached)" {
        "detached"
    } else {
        branch
    };
    let branch = sanitize_statusline_text(branch, 160);
    if branch.is_empty() {
        return None;
    }
    Some(OpenCodeGitSnapshot {
        branch,
        dirty: output.lines().any(|line| !line.starts_with('#')),
        cached: false,
    })
}

fn opencode_git_cache_path(cwd: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cwd.hash(&mut hasher);
    let runtime = rtrt_core::runtime_tmp_dir().ok()?;
    Some(runtime.join(format!("rtrt-opencode-git-{:016x}.json", hasher.finish())))
}

fn read_opencode_git_cache(cwd: &Path) -> Option<(OpenCodeGitSnapshot, u64)> {
    let path = opencode_git_cache_path(cwd)?;
    let raw = read_bounded_regular_file(&path, 4096)?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let recorded_at = value.get("ts")?.as_u64()?;
    let branch = sanitize_statusline_text(value.get("branch")?.as_str()?, 160);
    if branch.is_empty() {
        return None;
    }
    Some((
        OpenCodeGitSnapshot {
            branch,
            dirty: value.get("dirty")?.as_bool()?,
            cached: true,
        },
        recorded_at,
    ))
}

fn write_opencode_git_cache(cwd: &Path, snapshot: &OpenCodeGitSnapshot, recorded_at: u64) {
    let Some(path) = opencode_git_cache_path(cwd) else {
        return;
    };
    let value = serde_json::json!({
        "ts": recorded_at,
        "branch": snapshot.branch,
        "dirty": snapshot.dirty,
    });
    if let Ok(raw) = serde_json::to_vec(&value) {
        let _ = rtrt_core::write_private_file_atomic(&path, &raw);
    }
}

#[derive(Debug, Clone)]
struct StatuslineConfig {
    enabled_segments: Vec<String>,
    format: String,
    line2_format: String,
    line3_format: String,
    codex_check_timeout_ms: u64,
}

#[derive(Debug, Default)]
struct ClaudeStatusInput {
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    transcript_path: Option<PathBuf>,
    model_id: Option<String>,
    model_display_name: Option<String>,
    cache_pct: Option<u64>,
    // Authoritative numbers Claude Code provides directly in the status-line
    // payload, when present (newer CC versions). Preferred over transcript math.
    ctx_used_pct: Option<u64>,
    ctx_used_tokens: Option<u64>,
    ctx_window_size: Option<u64>,
    five_hour_pct: Option<u64>,
    five_hour_resets_at: Option<i64>,
    seven_day_pct: Option<u64>,
    seven_day_resets_at: Option<i64>,
}

#[derive(Debug, Default)]
struct GitStatusInfo {
    project: String,
    branch: String,
    wip_count: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct TranscriptTokenUsage {
    used_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    input_tokens: u64,
}

const DEFAULT_STATUSLINE_ENABLED_SEGMENTS: &[&str] = &[
    "project", "branch", "wip", "sess", "ctx", "cache", "opt", "model", "usage", "agents",
    "savings",
];
const LEGACY_STATUSLINE_ENABLED_SEGMENTS: &[&str] = &[
    "project", "branch", "wip", "sess", "ctx", "cache", "model", "usage", "codex", "savings",
];
const DEFAULT_STATUSLINE_FORMAT: &str =
    "{project} [{branch}] {wip} {sess} {ctx} {cache} {opt} {model} {agents}";
const LEGACY_STATUSLINE_FORMAT: &str = "{project} [{branch}] {wip} {sess} {ctx} {cache} {model}";
const PRE_AGENTS_STATUSLINE_FORMAT: &str =
    "{project} [{branch}] {wip} {sess} {ctx} {cache} {opt} {model}";
const DEFAULT_STATUSLINE_LINE2_FORMAT: &str = "{usage}";
const DEFAULT_STATUSLINE_LINE3_FORMAT: &str = "{savings}";
const PRE_AGENTS_STATUSLINE_LINE3_FORMAT: &str = "{agents} | {savings}";
const LEGACY_STATUSLINE_LINE3_FORMAT: &str = "{codex} | {savings}";
const DEFAULT_CODEX_CHECK_TIMEOUT_MS: u64 = 600;
/// Claude Code refreshes this often, so transcript parsing only considers the
/// newest JSONL records. This keeps statusline latency bounded on long sessions.
const TRANSCRIPT_RECENT_LINE_CAP: usize = 4_000;
/// Best-effort "today" session count uses a rolling day when local calendar
/// data is unavailable from std alone.
const SESSION_TODAY_WINDOW_SECS: u64 = 24 * 60 * 60;
const AGENTS_STATUS_CACHE_TTL_SECS: u64 = 30;

/// Context-window lookup used by `rtrt statusline`.
///
/// Entries are model-id prefixes because Claude Code can pass date-suffixed or
/// vendor-suffixed ids. Unknown models intentionally omit the ctx segment.
const STATUSLINE_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("claude-opus-4-8", 1_000_000),
    ("claude-opus-4", 200_000),
    ("claude-sonnet-4", 200_000),
    ("claude-haiku-4", 200_000),
    ("gpt-5", 400_000),
    ("gpt-4.1", 1_000_000),
    ("o3", 200_000),
    ("o4", 200_000),
];

fn print_statusline_badge() {
    let level = rtrt_core::read_output_style_level();
    if level.is_active() {
        print!("[OPT:{}]", level.as_str().to_ascii_uppercase());
    }
}

fn print_statusline(opts: StatuslineOptions) {
    let stdin_is_tty = std::io::stdin().is_terminal();
    if !opts.rich && stdin_is_tty {
        print_statusline_badge();
        return;
    }

    let mut raw = String::new();
    if !stdin_is_tty {
        let _ = std::io::stdin().read_to_string(&mut raw);
    }
    match build_statusline_output(&raw, opts.format) {
        Some(output) if !output.trim().is_empty() => println!("{output}"),
        _ => print_statusline_badge(),
    }
}

fn build_statusline_output(raw_stdin: &str, format_override: Option<String>) -> Option<String> {
    let input = parse_claude_status_input(raw_stdin);
    persist_claude_rate_limits(raw_stdin, epoch_secs());
    let cwd = input
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let repo_root = repo_root_from_cwd(&cwd);
    // Effective statusline config: the repo's per-project override ("Custom")
    // when set, else the global config ("Follow global", the default).
    let mut cfg = load_statusline_config(Some(&repo_root));
    if let Some(format) = format_override {
        cfg.format = format;
    }
    let git = load_git_status(&cwd);
    let transcript_usage = input
        .transcript_path
        .as_deref()
        .and_then(load_recent_transcript_usage);
    let mut segments = BTreeMap::new();
    // Compute the 5h / weekly window segment while `input` is still fully owned
    // (later inserts move its model fields).
    let usage_seg = usage_window_segment(&input);

    if let Some(git) = git {
        segments.insert("project".to_string(), git.project);
        segments.insert("branch".to_string(), git.branch);
        segments.insert("wip".to_string(), format!("wip:{}", git.wip_count));
    }
    segments.insert(
        "sess".to_string(),
        format!(
            "sess:{}",
            session_count_today(
                input.session_id.as_deref(),
                input.transcript_path.as_deref()
            )
        ),
    );
    // ctx: prefer Claude Code's authoritative `context_window` numbers; fall
    // back to a transcript-derived estimate only when they are absent.
    if let Some(pct) = input.ctx_used_pct {
        let window = input
            .ctx_window_size
            .or_else(|| input.model_id.as_deref().and_then(context_window_for_model));
        let seg = match (input.ctx_used_tokens, window) {
            (Some(used), Some(window)) if window > 0 => {
                format!(
                    "ctx:{pct}%({}/{})",
                    compact_count(used),
                    compact_count(window)
                )
            }
            _ => format!("ctx:{pct}%"),
        };
        segments.insert("ctx".to_string(), seg);
    }
    if let (Some(model), Some(usage)) = (input.model_id.as_deref(), transcript_usage) {
        if input.ctx_used_pct.is_none()
            && let Some(window) = context_window_for_model(model)
            && usage.used_tokens > 0
        {
            let pct = percentage_rounded(usage.used_tokens, window);
            segments.insert(
                "ctx".to_string(),
                format!(
                    "ctx:{}%({}/{})",
                    pct,
                    compact_count(usage.used_tokens),
                    compact_count(window)
                ),
            );
        }
        if let Some(pct) = input.cache_pct.or_else(|| transcript_cache_pct(usage)) {
            segments.insert("cache".to_string(), format!("cache:{pct}%"));
        }
    } else if let Some(pct) = input.cache_pct {
        segments.insert("cache".to_string(), format!("cache:{pct}%"));
    }
    if let Some(model) = input.model_display_name.or(input.model_id) {
        segments.insert("model".to_string(), model);
    }
    let opt_level = rtrt_core::read_output_style_level_for(Some(&repo_root));
    segments.insert("opt".to_string(), format!("opt:{}", opt_level.as_str()));
    if let Some(usage) = usage_seg {
        segments.insert("usage".to_string(), usage);
    }
    let agents = agents_segment(
        &repo_root,
        cfg.codex_check_timeout_ms,
        statusline_agents_width_budget(),
    );
    segments.insert("agents".to_string(), agents.clone());
    segments.insert("codex".to_string(), agents);
    let savings_project = rtrt_core::project_for_cwd(&cwd);
    segments.insert(
        "savings".to_string(),
        statusline_savings_segment(&savings_project, opt_level)
            .unwrap_or_else(|| format!("💯Σ:{}", total_savings_tokens())),
    );

    Some(render_statusline(&cfg, &segments))
}

fn render_statusline(cfg: &StatuslineConfig, segments: &BTreeMap<String, String>) -> String {
    let enabled = cfg
        .enabled_segments
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    [&cfg.format, &cfg.line2_format, &cfg.line3_format]
        .into_iter()
        .filter_map(|template| {
            let line = render_statusline_template(template, segments, &enabled);
            (!line.is_empty()).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_statusline_template(
    template: &str,
    segments: &BTreeMap<String, String>,
    enabled: &std::collections::BTreeSet<&str>,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &after[..end];
        if enabled.contains(key) {
            if let Some(value) = segments.get(key) {
                out.push_str(value);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    clean_statusline_line(&out)
}

fn clean_statusline_line(line: &str) -> String {
    let mut out = line.replace("[]", "").replace("()", "");
    loop {
        let next = out
            .replace("  ", " ")
            .replace(" | | ", " | ")
            .replace("| |", "|")
            .replace("[ ]", "")
            .replace("( )", "");
        if next == out {
            break;
        }
        out = next;
    }
    out.trim()
        .trim_matches('|')
        .trim()
        .trim_end_matches('[')
        .trim()
        .to_string()
}

fn load_statusline_config(repo: Option<&std::path::Path>) -> StatuslineConfig {
    // Per-project override ("Custom" mode) wins when the repo set one; otherwise
    // the project follows the global statusline (the default).
    if let Some(repo) = repo
        && let Ok(project) = rtrt_core::Config::load_project(repo)
        && let Some(section) = project.statusline_section_toml()
        && let Some(cfg) = parse_statusline_config(&section)
    {
        return upgrade_legacy_statusline_config(cfg);
    }
    let Some(path) = home_dir().map(|home| home.join(".rtrt").join("config.toml")) else {
        return StatuslineConfig::default();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return StatuslineConfig::default();
    };
    parse_statusline_config(&raw)
        .map(upgrade_legacy_statusline_config)
        .unwrap_or_default()
}

fn upgrade_legacy_statusline_config(mut cfg: StatuslineConfig) -> StatuslineConfig {
    let legacy_segments = LEGACY_STATUSLINE_ENABLED_SEGMENTS
        .iter()
        .map(|item| (*item).to_string())
        .collect::<Vec<_>>();
    if cfg.enabled_segments == legacy_segments {
        cfg.enabled_segments = DEFAULT_STATUSLINE_ENABLED_SEGMENTS
            .iter()
            .map(|item| (*item).to_string())
            .collect();
    }
    if cfg.format == LEGACY_STATUSLINE_FORMAT || cfg.format == PRE_AGENTS_STATUSLINE_FORMAT {
        cfg.format = DEFAULT_STATUSLINE_FORMAT.to_string();
    }
    if cfg.line3_format == LEGACY_STATUSLINE_LINE3_FORMAT
        || cfg.line3_format == PRE_AGENTS_STATUSLINE_LINE3_FORMAT
    {
        cfg.line3_format = DEFAULT_STATUSLINE_LINE3_FORMAT.to_string();
    }
    cfg
}

fn parse_statusline_config(raw: &str) -> Option<StatuslineConfig> {
    let mut cfg = StatuslineConfig::default();
    let mut in_statusline = false;
    for raw_line in raw.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_statusline = line == "[statusline]";
            continue;
        }
        if !in_statusline {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "enabled_segments" => {
                if let Some(items) = parse_toml_string_array(value) {
                    cfg.enabled_segments = items;
                }
            }
            "format" => {
                if let Some(value) = parse_toml_string(value) {
                    cfg.format = value;
                }
            }
            "line2_format" => {
                if let Some(value) = parse_toml_string(value) {
                    cfg.line2_format = value;
                }
            }
            "line3_format" => {
                if let Some(value) = parse_toml_string(value) {
                    cfg.line3_format = value;
                }
            }
            "codex_check_timeout_ms" => {
                if let Ok(value) = value.parse::<u64>() {
                    cfg.codex_check_timeout_ms = value;
                }
            }
            _ => {}
        }
    }
    Some(cfg)
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn parse_toml_string(value: &str) -> Option<String> {
    let value = value.trim();
    if !(value.starts_with('"') && value.ends_with('"')) {
        return None;
    }
    let inner = &value[1..value.len().saturating_sub(1)];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => return None,
        }
    }
    Some(out)
}

fn parse_toml_string_array(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    if !(value.starts_with('[') && value.ends_with(']')) {
        return None;
    }
    let inner = &value[1..value.len().saturating_sub(1)];
    let mut items = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let end = quoted_value_end(rest)?;
        items.push(parse_toml_string(&rest[..=end])?);
        rest = rest[end + 1..].trim_start();
        if rest.is_empty() {
            break;
        }
        if !rest.starts_with(',') {
            return None;
        }
        rest = rest[1..].trim_start();
    }
    Some(items)
}

fn quoted_value_end(value: &str) -> Option<usize> {
    if !value.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    for (idx, ch) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(idx);
        }
    }
    None
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            enabled_segments: DEFAULT_STATUSLINE_ENABLED_SEGMENTS
                .iter()
                .map(|item| (*item).to_string())
                .collect(),
            format: DEFAULT_STATUSLINE_FORMAT.to_string(),
            line2_format: DEFAULT_STATUSLINE_LINE2_FORMAT.to_string(),
            line3_format: DEFAULT_STATUSLINE_LINE3_FORMAT.to_string(),
            codex_check_timeout_ms: DEFAULT_CODEX_CHECK_TIMEOUT_MS,
        }
    }
}

fn persist_claude_rate_limits(raw: &str, captured_at: u64) {
    let Some(rate_limits) = extract_official_claude_rate_limits(raw, captured_at) else {
        return;
    };
    let path = claude_rate_limit_cache_override()
        .or_else(|| default_claude_rate_limit_cache_paths(true).map(|(current, _legacy)| current));
    let Some(path) = path else {
        return;
    };
    let _ = write_claude_rate_limit_cache(&path, &rate_limits);
}

fn extract_official_claude_rate_limits(raw: &str, captured_at: u64) -> Option<ClaudeRateLimits> {
    let value = serde_json::from_str::<serde_json::Value>(raw.trim()).ok()?;
    let rate_limits = value.get("rate_limits")?.as_object()?;
    let five_hour = rate_limits
        .get("five_hour")
        .and_then(parse_official_claude_rate_limit_window);
    let seven_day = rate_limits
        .get("seven_day")
        .and_then(parse_official_claude_rate_limit_window);
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    Some(ClaudeRateLimits {
        captured_at,
        five_hour,
        seven_day,
    })
}

fn parse_official_claude_rate_limit_window(
    value: &serde_json::Value,
) -> Option<ClaudeRateLimitWindow> {
    let used_percentage = value.get("used_percentage")?.as_f64()?;
    let resets_at = value.get("resets_at")?.as_u64()?;
    if !used_percentage.is_finite()
        || !(0.0..=100.0).contains(&used_percentage)
        || resets_at > i64::MAX as u64
    {
        return None;
    }
    Some(ClaudeRateLimitWindow {
        used_percentage,
        resets_at,
    })
}

fn write_claude_rate_limit_cache(
    path: &Path,
    rate_limits: &ClaudeRateLimits,
) -> std::io::Result<()> {
    let mut windows = serde_json::Map::new();
    if let Some(window) = &rate_limits.five_hour {
        windows.insert("five_hour".into(), rate_limit_window_json(window));
    }
    if let Some(window) = &rate_limits.seven_day {
        windows.insert("seven_day".into(), rate_limit_window_json(window));
    }
    if windows.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Claude rate-limit cache has no windows",
        ));
    }
    let cache = serde_json::json!({
        "v": CLAUDE_RATE_LIMIT_CACHE_VERSION,
        "captured_at": rate_limits.captured_at,
        "windows": windows,
    });
    let raw = serde_json::to_vec(&cache).map_err(std::io::Error::other)?;
    rtrt_core::write_private_file_atomic(path, &raw)
}

fn parse_claude_status_input(raw: &str) -> ClaudeStatusInput {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return ClaudeStatusInput::default();
    };
    let workspace = value.get("workspace").unwrap_or(&serde_json::Value::Null);
    let cwd = json_string(&value, "cwd")
        .or_else(|| json_string(workspace, "current_dir"))
        .or_else(|| json_string(workspace, "project_dir"))
        .map(PathBuf::from);
    let model = value.get("model").unwrap_or(&serde_json::Value::Null);
    let null = serde_json::Value::Null;
    let ctx_window = value.get("context_window").unwrap_or(&null);
    let rate_limits = value.get("rate_limits").unwrap_or(&null);
    let five_hour = rate_limits.get("five_hour").unwrap_or(&null);
    let seven_day = rate_limits.get("seven_day").unwrap_or(&null);
    ClaudeStatusInput {
        session_id: json_string(&value, "session_id"),
        cwd,
        transcript_path: json_string(&value, "transcript_path").map(PathBuf::from),
        model_id: json_string(model, "id"),
        model_display_name: json_string(model, "display_name"),
        cache_pct: value.get("cost").and_then(cache_pct_from_value),
        ctx_used_pct: json_round_pct(ctx_window, "used_percentage"),
        ctx_used_tokens: json_any_u64(ctx_window, "total_input_tokens"),
        ctx_window_size: json_any_u64(ctx_window, "context_window_size"),
        five_hour_pct: json_round_pct(five_hour, "used_percentage"),
        five_hour_resets_at: json_epoch(five_hour, "resets_at"),
        seven_day_pct: json_round_pct(seven_day, "used_percentage"),
        seven_day_resets_at: json_epoch(seven_day, "resets_at"),
    }
}

/// Read a reset timestamp that Claude Code may send either as a Unix epoch
/// (integer or float seconds) or as an RFC 3339 string. Returns epoch seconds.
fn json_epoch(value: &serde_json::Value, key: &str) -> Option<i64> {
    let v = value.get(key)?;
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    v.as_str().and_then(rfc3339_to_epoch)
}

/// Read a percentage field (already 0–100) and round it to a whole number.
fn json_round_pct(value: &serde_json::Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v.round().min(9_999.0) as u64)
}

/// Read an integer-ish token count (accepts JSON int or float).
fn json_any_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
}

/// Parse an RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`) to a
/// Unix epoch in seconds. Minimal, dependency-free; returns `None` on anything
/// it cannot read.
fn rfc3339_to_epoch(s: &str) -> Option<i64> {
    let g = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (year, month, day) = (g(0, 4)?, g(5, 7)?, g(8, 10)?);
    let (hour, min, sec) = (g(11, 13)?, g(14, 16)?, g(17, 19)?);
    // days_from_civil (Howard Hinnant)
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut epoch = days * 86_400 + hour * 3_600 + min * 60 + sec;
    // Apply any explicit timezone offset (Z = +00:00).
    let tz = &s[19.min(s.len())..];
    let tz = tz.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(pos) = tz.find(['+', '-'])
        && let Some(off) = tz.get(pos + 1..)
        && off.len() >= 5
        && let (Some(oh), Some(om)) = (
            off.get(0..2).and_then(|x| x.parse::<i64>().ok()),
            off.get(3..5).and_then(|x| x.parse::<i64>().ok()),
        )
    {
        let sign = if tz.as_bytes()[pos] == b'-' { -1 } else { 1 };
        epoch -= sign * (oh * 3_600 + om * 60);
    }
    Some(epoch)
}

/// Compact remaining-time label, e.g. `3h12m` or `7m` or `now`.
fn humanize_remaining(secs: i64) -> String {
    if secs <= 0 {
        return "now".to_string();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else {
        format!("{mins}m")
    }
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(ToString::to_string)
}

fn load_git_status(cwd: &Path) -> Option<GitStatusInfo> {
    let root = run_command_timeout(
        "git",
        &[
            "-C",
            cwd.to_string_lossy().as_ref(),
            "rev-parse",
            "--show-toplevel",
        ],
        std::time::Duration::from_millis(120),
    )?;
    let root_path = PathBuf::from(root.trim());
    let project = root_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())?
        .to_string();
    let branch = run_command_timeout(
        "git",
        &[
            "-C",
            cwd.to_string_lossy().as_ref(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ],
        std::time::Duration::from_millis(120),
    )?
    .trim()
    .to_string();
    if branch.is_empty() {
        return None;
    }
    let status = run_command_timeout(
        "git",
        &["-C", cwd.to_string_lossy().as_ref(), "status", "--short"],
        std::time::Duration::from_millis(160),
    )
    .unwrap_or_default();
    Some(GitStatusInfo {
        project,
        branch,
        wip_count: status
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    })
}

fn run_command_timeout(
    binary: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<String> {
    let mut child = std::process::Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok()?;
                if status.success() {
                    return String::from_utf8(output.stdout).ok();
                }
                return None;
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(_) => return None,
        }
    }
}

fn load_recent_transcript_usage(path: &Path) -> Option<TranscriptTokenUsage> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut recent = std::collections::VecDeque::with_capacity(TRANSCRIPT_RECENT_LINE_CAP);
    for line in reader.lines().map_while(Result::ok) {
        if recent.len() == TRANSCRIPT_RECENT_LINE_CAP {
            recent.pop_front();
        }
        recent.push_back(line);
    }
    // Use the most recent line that carries a usage object: the CURRENT context
    // footprint, not a cumulative sum across turns. Summing over many lines
    // re-counts the cached context that is re-read every turn (~the whole window
    // each time), which blows the percentage far past 100% (e.g. 52535%).
    for line in recent.iter().rev() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            let mut usage = TranscriptTokenUsage::default();
            collect_token_usage(&value, &mut usage);
            if usage.used_tokens > 0 {
                return Some(usage);
            }
        }
    }
    None
}

fn collect_token_usage(value: &serde_json::Value, usage: &mut TranscriptTokenUsage) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(raw_usage) = map.get("usage") {
                add_usage_object(raw_usage, usage);
            }
            for (key, value) in map {
                if key != "usage" {
                    collect_token_usage(value, usage);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_token_usage(item, usage);
            }
        }
        _ => {}
    }
}

fn add_usage_object(value: &serde_json::Value, usage: &mut TranscriptTokenUsage) {
    let input = token_field(value, "input_tokens").or_else(|| token_field(value, "prompt_tokens"));
    let cache_read = token_field(value, "cache_read_input_tokens")
        .or_else(|| token_field(value, "cached_tokens"));
    let cache_write = token_field(value, "cache_creation_input_tokens");
    // Context footprint = the prompt side (fresh input + cached context reused +
    // newly written cache). Output is the generation, not occupied context, so
    // it is excluded from the context-window percentage.
    let specific_total = input
        .unwrap_or(0)
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(cache_write.unwrap_or(0));
    let total = if specific_total > 0 {
        specific_total
    } else {
        token_field(value, "total_tokens").unwrap_or(0)
    };
    usage.used_tokens = usage.used_tokens.saturating_add(total);
    usage.input_tokens = usage.input_tokens.saturating_add(input.unwrap_or(0));
    usage.cache_read_tokens = usage
        .cache_read_tokens
        .saturating_add(cache_read.unwrap_or(0));
    usage.cache_write_tokens = usage
        .cache_write_tokens
        .saturating_add(cache_write.unwrap_or(0));
}

fn token_field(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn context_window_for_model(model_id: &str) -> Option<u64> {
    STATUSLINE_CONTEXT_WINDOWS
        .iter()
        .find_map(|(prefix, window)| model_id.starts_with(prefix).then_some(*window))
}

fn percentage_rounded(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    numerator
        .saturating_mul(100)
        .saturating_add(denominator / 2)
        / denominator
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

fn cache_pct_from_value(value: &serde_json::Value) -> Option<u64> {
    if let Some(pct) = token_field(value, "cache_pct") {
        return Some(pct.min(100));
    }
    if let Some(pct) = value
        .get("cache_hit_rate")
        .and_then(serde_json::Value::as_f64)
    {
        return f64_to_percentage(pct);
    }
    let usage = {
        let mut usage = TranscriptTokenUsage::default();
        add_usage_object(value, &mut usage);
        usage
    };
    transcript_cache_pct(usage)
}

fn transcript_cache_pct(usage: TranscriptTokenUsage) -> Option<u64> {
    let denom = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    (usage.cache_read_tokens > 0 && denom > 0)
        .then(|| percentage_rounded(usage.cache_read_tokens, denom))
}

fn f64_to_percentage(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let pct = if value <= 1.0 { value * 100.0 } else { value };
    Some(pct.round().clamp(0.0, 100.0) as u64)
}

fn session_count_today(session_id: Option<&str>, transcript_path: Option<&Path>) -> usize {
    let Some(path) = transcript_path else {
        return usize::from(session_id.is_some()).max(1);
    };
    let Some(parent) = path.parent() else {
        return usize::from(session_id.is_some()).max(1);
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return usize::from(session_id.is_some()).max(1);
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(elapsed) = modified.elapsed() else {
            continue;
        };
        if elapsed.as_secs() <= SESSION_TODAY_WINDOW_SECS {
            count = count.saturating_add(1);
        }
    }
    count.max(usize::from(session_id.is_some())).max(1)
}

/// Render the `5h:X% ↻… | wk:Y% ↻…` rate-limit window segment from the values
/// Claude Code supplies in the status-line payload. Returns `None` when neither
/// window is present (so the segment is hidden rather than showing `n/a`).
fn usage_window_segment(input: &ClaudeStatusInput) -> Option<String> {
    if input.five_hour_pct.is_none() && input.seven_day_pct.is_none() {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64);
    // ∅ = projected time to exhaustion. Prefer the responsive recent burn rate
    // (from the rolling log); fall back to the average rate over the current
    // window so it shows from the first render instead of needing history.
    let (five_recent, seven_recent) = match now {
        Some(now) => usage_burn_etas(now, input.five_hour_pct, input.seven_day_pct),
        None => (None, None),
    };
    let combined =
        |recent: Option<i64>, pct: Option<u64>, resets_at: Option<i64>, window_secs: i64| {
            recent.or_else(|| average_rate_eta(now?, pct?, resets_at, window_secs))
        };
    let five_eta = combined(
        five_recent,
        input.five_hour_pct,
        input.five_hour_resets_at,
        FIVE_HOUR_SECS,
    );
    let seven_eta = combined(
        seven_recent,
        input.seven_day_pct,
        input.seven_day_resets_at,
        SEVEN_DAY_SECS,
    );
    let window = |pct: Option<u64>, resets_at: Option<i64>, eta: Option<i64>| -> Option<String> {
        let pct = pct?;
        let reset = match (now, resets_at) {
            (Some(now), Some(reset)) => format!(" ↻{}", humanize_remaining(reset - now)),
            _ => String::new(),
        };
        let burn = eta
            .map(|secs| format!(" ∅{}", humanize_remaining(secs)))
            .unwrap_or_default();
        Some(format!("{pct}%{reset}{burn}"))
    };
    let five = window(input.five_hour_pct, input.five_hour_resets_at, five_eta);
    let seven = window(input.seven_day_pct, input.seven_day_resets_at, seven_eta);
    if five.is_none() && seven.is_none() {
        return None;
    }
    let five = five.unwrap_or_else(|| "—".to_string());
    let seven = seven.unwrap_or_else(|| "—".to_string());
    Some(format!("5h:{five} | wk:{seven}"))
}

const STATUSLINE_USAGE_LOG_CAP: usize = 4000;
const FIVE_HOUR_SECS: i64 = 5 * 3_600;
const SEVEN_DAY_SECS: i64 = 7 * 86_400;

fn statusline_usage_log_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".rtrt").join("statusline-usage.tsv"))
}

/// Average-rate exhaustion ETA: assume the current percentage accrued evenly
/// since the window opened (`resets_at - window_secs`) and project the rest at
/// that average rate. This is the always-available fallback when there is no
/// recent burn-rate sample yet. `None` at 0% (nothing to project) or without a
/// reset time to anchor the window.
fn average_rate_eta(now: i64, pct: u64, resets_at: Option<i64>, window_secs: i64) -> Option<i64> {
    if pct == 0 {
        return None;
    }
    let reset = resets_at?;
    let elapsed = now - (reset - window_secs);
    if elapsed <= 0 {
        return None;
    }
    let rate = pct as f64 / elapsed as f64;
    if rate <= 0.0 {
        return None;
    }
    let remaining = (100.0 - pct as f64).max(0.0);
    Some((remaining / rate) as i64)
}

/// Append the current 5h / weekly percentages to a small rolling log and
/// project, from the recent burn rate, how long until each window reaches
/// 100%. Returns `(five_hour_eta_secs, seven_day_eta_secs)`; `None` for a
/// window whose usage is flat/falling or lacks enough history.
fn usage_burn_etas(
    now: i64,
    five_pct: Option<u64>,
    seven_pct: Option<u64>,
) -> (Option<i64>, Option<i64>) {
    let Some(path) = statusline_usage_log_path() else {
        return (None, None);
    };
    let mut rows: Vec<(i64, f64, f64)> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            if let (Some(ts), Some(a), Some(b)) = (fields.next(), fields.next(), fields.next())
                && let (Ok(ts), Ok(a), Ok(b)) =
                    (ts.parse::<i64>(), a.parse::<f64>(), b.parse::<f64>())
            {
                rows.push((ts, a, b));
            }
        }
    }
    // Append the current sample (-1 marks an absent window).
    let cur = |p: Option<u64>| p.map(|v| v as f64).unwrap_or(-1.0);
    rows.push((now, cur(five_pct), cur(seven_pct)));
    if rows.len() > STATUSLINE_USAGE_LOG_CAP {
        let drop = rows.len() - STATUSLINE_USAGE_LOG_CAP;
        rows.drain(0..drop);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut out = String::from("# epoch\tfive_pct\tseven_pct\n");
    for (ts, a, b) in &rows {
        out.push_str(&format!("{ts}\t{a}\t{b}\n"));
    }
    let _ = std::fs::write(&path, out);

    let eta = |cur_pct: Option<u64>, idx: usize| -> Option<i64> {
        let cur_pct = cur_pct? as f64;
        let val = |r: &(i64, f64, f64)| if idx == 0 { r.1 } else { r.2 };
        let window_start = now - 1_800; // 30 min look-back
        // Earliest in-window sample with a lower percentage (usage rose); fall
        // back to the earliest lower sample anywhere in the log.
        let base = rows
            .iter()
            .find(|r| r.0 >= window_start && val(r) >= 0.0 && val(r) < cur_pct)
            .or_else(|| rows.iter().find(|r| val(r) >= 0.0 && val(r) < cur_pct))
            .copied()?;
        let dt = now - base.0;
        let dpct = cur_pct - val(&base);
        if dt < 30 || dpct <= 0.0 {
            return None;
        }
        let rate_per_sec = dpct / dt as f64;
        if rate_per_sec <= 0.0 {
            return None;
        }
        let remaining = (100.0 - cur_pct).max(0.0);
        Some((remaining / rate_per_sec) as i64)
    };
    (eta(five_pct, 0), eta(seven_pct, 1))
}

fn agents_segment(repo: &std::path::Path, timeout_ms: u64, width_budget: usize) -> String {
    // Resolve the repo's effective config up front (cheap file read) so the
    // detection thread filters the agents list by the per-project
    // `[agents]`/`[providers]` enable map. No repo / no override → global
    // behavior, identical to before.
    let cfg = rtrt_core::Config::load_effective(Some(repo)).unwrap_or_default();
    // The cache key folds in the repo *and* the enable/active fingerprint so
    // editing `<repo>/.rtrt/config.toml` (e.g. `codex = false`) invalidates a
    // stale cached segment instead of masking the override.
    let cache_key = agents_cache_key(repo, &cfg);
    if let Some(cached) = read_agents_status_cache(cache_key) {
        return cached;
    }
    // The full detection thread can blow the timeout (it execs each agent's
    // `--version`), so the fallback `cheap_statusline_agents` must apply the
    // same per-project opt-outs — otherwise a disabled agent reappears whenever
    // detection is slow. Keep a copy of the agent enable map for that path.
    let agents = cfg.agents.clone();
    let timeout = std::time::Duration::from_millis(timeout_ms.max(1));
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("rtrt-statusline-agents".into())
        .spawn(move || {
            let names = detected_statusline_agents(cfg);
            let _ = tx.send(names);
        });
    let names = rx
        .recv_timeout(timeout)
        .unwrap_or_else(|_| cheap_statusline_agents(&agents));
    let segment = format_agents_segment(&names, width_budget);
    write_agents_status_cache(cache_key, &segment);
    segment
}

/// Cache key for the agents segment: the repo path plus the per-project agent
/// and provider enable maps (and active provider). A config edit changes the
/// key, so a stale cache never masks a freshly disabled/enabled agent.
fn agents_cache_key(repo: &std::path::Path, cfg: &rtrt_core::Config) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut hasher);
    for (name, enabled) in &cfg.agents.enabled {
        name.hash(&mut hasher);
        enabled.hash(&mut hasher);
    }
    for (name, enabled) in &cfg.providers.enabled {
        name.hash(&mut hasher);
        enabled.hash(&mut hasher);
    }
    cfg.providers.active.hash(&mut hasher);
    hasher.finish()
}

fn detected_statusline_agents(cfg: rtrt_core::Config) -> Vec<String> {
    let mut names = Vec::new();
    for tool in rtrt_core::detect_tools_with_config(cfg)
        .into_iter()
        .filter(|tool| {
            matches!(tool.kind, ToolKind::CodingAgent | ToolKind::LocalRuntime)
                && tool.installed
                && tool.enabled
        })
    {
        let name = short_tool_name(&tool.name).to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn short_tool_name(name: &str) -> &str {
    match name {
        "gh-copilot" => "gh",
        "llama" => "llama",
        other => other,
    }
}

fn cheap_statusline_agents(agents: &rtrt_core::AgentsConfig) -> Vec<String> {
    // (short name, canonical descriptor name, candidate binaries). The
    // canonical name is what `[agents]` keys on (e.g. `gh-copilot`); the short
    // name is what the segment displays (`gh`).
    const CANDIDATES: &[(&str, &str, &[&str])] = &[
        ("claude", "claude", &["claude"]),
        ("codex", "codex", &["codex"]),
        ("opencode", "opencode", &["opencode"]),
        ("ollama", "ollama", &["ollama"]),
        ("aider", "aider", &["aider"]),
        ("cursor", "cursor", &["cursor-agent", "cursor"]),
        ("gemini", "gemini", &["gemini"]),
        ("gh", "gh-copilot", &["gh"]),
    ];
    CANDIDATES
        .iter()
        // Drop agents the project explicitly disabled so the fast fallback
        // honors the same override as full detection. An absent entry keeps the
        // PATH-probe default (enabled when the binary is present).
        .filter(|(_short, canonical, _bins)| agents.enabled_override(canonical) != Some(false))
        .filter(|(_short, _canonical, bins)| bins.iter().any(|bin| binary_on_path(bin)))
        .map(|(short, _canonical, _bins)| (*short).to_string())
        .collect()
}

fn binary_on_path(binary: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        executable_path_candidates(&dir, binary)
            .into_iter()
            .any(|path| is_executable_path(&path))
    })
}

fn executable_path_candidates(dir: &Path, binary: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let path = Path::new(binary);
        if path.extension().is_some() {
            return vec![dir.join(binary)];
        }
        let exts = std::env::var_os("PATHEXT")
            .map(|value| {
                value
                    .to_string_lossy()
                    .split(';')
                    .filter(|ext| !ext.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]);
        exts.into_iter()
            .map(|ext| dir.join(format!("{binary}{ext}")))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![dir.join(binary)]
    }
}

fn is_executable_path(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & UNIX_EXECUTE_BITS != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn statusline_agents_width_budget() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|columns| (columns / 3).clamp(18, 56))
        .unwrap_or(40)
}

fn format_agents_segment(names: &[String], width_budget: usize) -> String {
    if names.is_empty() {
        return "🤖 none".to_string();
    }
    let prefix = "🤖 ";
    let mut shown = Vec::new();
    let mut used = prefix.chars().count();
    for (idx, name) in names.iter().enumerate() {
        let sep = usize::from(idx > 0);
        let candidate_len = name.chars().count() + sep;
        let remaining = names.len().saturating_sub(shown.len() + 1);
        let suffix_len = if remaining > 0 {
            format!("+{remaining}").chars().count() + 1
        } else {
            0
        };
        if !shown.is_empty() && used + candidate_len + suffix_len > width_budget {
            break;
        }
        if shown.is_empty() && used + candidate_len + suffix_len > width_budget {
            shown.push(name.clone());
            break;
        }
        shown.push(name.clone());
        used += candidate_len;
    }
    let hidden = names.len().saturating_sub(shown.len());
    let mut out = format!("{prefix}{}", shown.join("·"));
    if hidden > 0 {
        out.push('·');
        out.push_str(&format!("+{hidden}"));
    }
    out
}

fn read_agents_status_cache(key: u64) -> Option<String> {
    let path = agents_status_cache_path(key).ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta.modified().ok()?;
    if modified.elapsed().ok()?.as_secs() > AGENTS_STATUS_CACHE_TTL_SECS {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed == "🤖 none" {
        return None;
    }
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn write_agents_status_cache(key: u64, status: &str) {
    if let Ok(runtime_tmp_dir) = rtrt_core::runtime_tmp_dir() {
        let _ = write_agents_status_cache_in(&runtime_tmp_dir, key, status);
    }
}

fn write_agents_status_cache_in(
    runtime_tmp_dir: &Path,
    key: u64,
    status: &str,
) -> std::io::Result<()> {
    let path = agents_status_cache_path_in(runtime_tmp_dir, key);
    rtrt_core::write_private_file_atomic(&path, status.as_bytes())
}

/// The agents-status cache is keyed per repo + per-project enable fingerprint
/// (see [`agents_cache_key`]) so a project's `[agents]` override (e.g.
/// `codex = false`) is not masked by another repo's — or a pre-edit — cached
/// agents list.
fn agents_status_cache_path(key: u64) -> Result<PathBuf> {
    Ok(agents_status_cache_path_in(
        &rtrt_core::runtime_tmp_dir()?,
        key,
    ))
}

fn agents_status_cache_path_in(runtime_tmp_dir: &Path, key: u64) -> PathBuf {
    runtime_tmp_dir.join(format!("rtrt-agents-status-{key:016x}.cache"))
}

fn total_savings_tokens() -> u64 {
    let path = proxy_stats::default_path();
    if !path.exists() {
        return 0;
    }
    proxy_stats::load_summary(None, None, false)
        .ok()
        .map(|summary| estimated_tokens(summary.saved_chars))
        .unwrap_or(0)
}

const SAVINGS_CACHE_TTL_SECS: u64 = 60;

fn savings_cache_path(project: &str) -> PathBuf {
    let safe: String = project
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    home_dir()
        .map(|home| {
            home.join(".rtrt")
                .join(format!("statusline-savings-{safe}.cache"))
        })
        .unwrap_or_else(|| PathBuf::from(format!(".rtrt/statusline-savings-{safe}.cache")))
}

fn read_savings_cache(project: &str) -> Option<String> {
    let path = savings_cache_path(project);
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    if modified.elapsed().ok()?.as_secs() > SAVINGS_CACHE_TTL_SECS {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    (!raw.trim().is_empty()).then(|| raw.trim().to_string())
}

fn write_savings_cache(project: &str, segment: &str) {
    let path = savings_cache_path(project);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, segment);
}

fn savings_pct(saved: u64, original: u64) -> u64 {
    if original == 0 {
        return 0;
    }
    (saved.saturating_mul(100).saturating_add(original / 2) / original).min(100)
}

/// Per-surface reduction breakdown for status-line line 3:
/// `📝opt:X% 🧠mem:N ⚡cmd:Y% 💯Σ:Z%` — Output Optimizer reduction, Memory
/// saves, Command Optimizer reduction, and the overall reduction. Cached for
/// [`SAVINGS_CACHE_TTL_SECS`] so the per-render cost stays negligible.
fn statusline_savings_segment(project: &str, opt_level: OutputStyleLevel) -> Option<String> {
    if let Some(cached) = read_savings_cache(project) {
        return Some(cached);
    }
    let segment = compute_statusline_savings(project, opt_level);
    if let Some(segment) = &segment {
        write_savings_cache(project, segment);
    }
    segment
}

fn compute_statusline_savings(project: &str, opt_level: OutputStyleLevel) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    // Output Optimizer: terse mode is prompt-injection — no before/after to
    // measure — so it is shown as its active level, never a fabricated percent.
    // Its measurable compress output lands in the memory store, so that reduction
    // is already counted under Memory below.
    if opt_level.is_active() {
        parts.push(format!("📝opt:{}", opt_level.as_str()));
    }
    // Memory: storage reduction — what it trims/redacts/dedups before storing.
    // This is an internal storage efficiency, shown as its own pillar; it is
    // NOT folded into Σ (Σ is agent-token savings, below).
    let (mem_saved, mem_base) = memory_savings_for_statusline(project);
    if mem_base > 0 {
        parts.push(format!("🧠mem:{}%", savings_pct(mem_saved, mem_base)));
    }
    // Command Optimizer: EFFECTIVE reduction over runs that actually filtered,
    // excluding passthroughs (a bare grep, or a cat of an already-terse file)
    // that otherwise dilute the rate toward zero.
    let (cmd_saved, cmd_input) =
        proxy_stats::load_effective_reduction(Some(project)).unwrap_or((0, 0));
    if cmd_input > 0 {
        parts.push(format!("⚡cmd:{}%", savings_pct(cmd_saved, cmd_input)));
    }
    // Σ = agent-token savings: tokens kept OUT of the model context — Command
    // Optimizer filtering plus recall reuse (recalled instead of re-derived).
    // Memory storage compression is internal (not model tokens) and terse mode
    // is unmeasured, so both are excluded here.
    let recall = read_recall_savings(project);
    let sigma_saved = cmd_saved.saturating_add(recall);
    let sigma_base = cmd_input.saturating_add(recall);
    if sigma_base > 0 {
        parts.push(format!("💯Σ:{}%", savings_pct(sigma_saved, sigma_base)));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Memory STORAGE reduction for `project`, as `(saved_chars, original_chars)`:
/// original (`body_full`) vs stored (`body`) via a cheap SQL aggregate. This is
/// the internal storage efficiency shown as the Memory pillar; recall reuse is
/// counted separately in Σ (agent-token savings), not here.
fn memory_savings_for_statusline(project: &str) -> (u64, u64) {
    let (original, stored) = current_project_identity()
        .ok()
        .filter(|identity| project_claim_matches(identity, project))
        .and_then(|identity| {
            let path = rtrt_core::project_memory_db_path(&identity).ok()?;
            path.exists().then_some(identity)
        })
        .and_then(|identity| {
            let slug = identity.slug().to_string();
            MemoryStore::open_project(&identity)
                .ok()?
                .storage_reduction(&slug)
                .ok()
        })
        .unwrap_or((0, 0));
    (original.saturating_sub(stored), original)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Best-effort HYBRID recall for the auto-recall hook.
///
/// Returns `Some(recall)` only when a hybrid (BM25 + dense-vector RRF) recall
/// completed successfully within `timeout`. Returns `None` — the caller's
/// signal to fall back to pure `recall_bm25` — when any gate is unmet:
/// embeddings disabled, no meaningful coverage, embedder cannot be built, the
/// query embedding errors, or the attempt exceeds `timeout` (slow/unreachable
/// Ollama). The prompt is NEVER blocked beyond `timeout`: the hybrid work runs
/// on a detached worker thread and we stop waiting on it once `timeout` lapses.
///
/// When embeddings are disabled (the LLM-free user) this returns `None`
/// immediately with zero LLM/Ollama traffic, so the recall hook stays pure
/// BM25.
///
/// `bm25_query` and `semantic_text` are intentionally separate: `bm25_query`
/// should be a [`rtrt_memory::sanitize_fts_query`] OR-join (FTS5 needs it —
/// implicit AND on a raw sentence rarely matches a terse memory row), while
/// `semantic_text` should be the raw, un-sanitized prompt (the embedder needs
/// real natural language, not a keyword bag, to produce a meaningful dense
/// vector). The prompt's meaning-free twin
/// ([`rtrt_memory::null_probe_text`]) rides along in the same embed call so
/// the caller can calibrate the similarity bar without a second round-trip.
/// Set `RTRT_RECALL_DEBUG=1` to log the gate/timing decision to stderr (never
/// stdout, so it can never leak into the injected context).
fn try_hybrid_recall(
    identity: &ProjectIdentity,
    project: &str,
    bm25_query: &str,
    semantic_text: &str,
    limit: usize,
    timeout: std::time::Duration,
) -> Option<rtrt_memory::HybridRecall> {
    let debug = std::env::var_os("RTRT_RECALL_DEBUG").is_some();
    let t0 = std::time::Instant::now();
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    // Gates 1+2 (embeddings enabled, meaningful project coverage) are shared
    // with the MCP server's `memory_recall` / `memory_smart_search` via
    // `rtrt_memory::hybrid_recall_ready` — a cheap local config + SQL read, no
    // network, so it's checked before touching Ollama at all.
    let coverage_store = MemoryStore::open_project(identity).ok()?;
    let ready = rtrt_memory::hybrid_recall_ready(&coverage_store, project, &cfg);
    if debug {
        eprintln!(
            "rtrt recall debug: hybrid_recall_ready={ready} embeddings_enabled={} gate_check={:?}",
            cfg.embeddings.is_enabled(),
            t0.elapsed()
        );
    }
    if !ready {
        return None;
    }
    drop(coverage_store);

    // Gate 3: build the embedder (same shared resolution: embeddings.base_url
    // → auto_compress.base_url → Ollama default).
    let embedder = rtrt_memory::hybrid_embedder_from_config(&cfg);

    // Run the hybrid recall on a worker thread bounded by `timeout`. The thread
    // opens its OWN MemoryStore on the WAL db (concurrent reads are fine) and
    // its own embedder, so nothing non-Send crosses the boundary. We wait on a
    // bounded channel: if Ollama is slow/unreachable we stop waiting after
    // `timeout` and the caller falls back to BM25. The worker is detached and
    // simply finishes (or errors) on its own; its result is discarded.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<rtrt_memory::HybridRecall>>(1);
    let identity = identity.clone();
    let project = project.to_string();
    let bm25_query = bm25_query.to_string();
    let semantic_text = semantic_text.to_string();
    std::thread::spawn(move || {
        let result = (|| {
            let store = MemoryStore::open_project(&identity).ok()?;
            let null_probe = rtrt_memory::null_probe_text(&semantic_text);
            store
                .recall_hybrid_scored(
                    &project,
                    rtrt_memory::HybridQuery {
                        bm25: &bm25_query,
                        semantic: &semantic_text,
                        null_probe: Some(&null_probe),
                        limit,
                    },
                    &embedder,
                )
                .ok()
        })();
        // Ignore send errors: the receiver may have already timed out and gone.
        let _ = tx.send(result);
    });

    // `recv_timeout` returns Err on both timeout and a dropped sender; either
    // way we fall back. A successful hybrid yields `Some(recall)`; an inner
    // error yields `Some(None)` which we also treat as a fall-back signal.
    let outcome = rx.recv_timeout(timeout);
    if debug {
        let status = match &outcome {
            Ok(Some(recall)) => format!(
                "ok hits={} bar={:?} null={:?} bg={:?}",
                recall.hits.len(),
                recall.similarity_bar(),
                recall.null_similarity,
                recall.background,
            ),
            Ok(None) => "inner-error".to_string(),
            Err(_) => "timeout-or-disconnected".to_string(),
        };
        eprintln!(
            "rtrt recall debug: hybrid outcome={status} total_elapsed={:?} timeout_budget={timeout:?}",
            t0.elapsed()
        );
    }
    match outcome {
        Ok(Some(recall)) => Some(recall),
        _ => None,
    }
}

/// Short bound for the opportunistic embed sweep triggered right after `memory
/// save` — the CLI process exits shortly after printing its result, so this
/// can't wait as long as an interactive recall; it's an "if it's fast, take
/// the win" courtesy call, not a guarantee. Any rows embedded before the
/// timeout are already committed (each is written as it's stored), so a
/// partial sweep still makes real progress.
const OPPORTUNISTIC_EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Best-effort incremental embed sweep after `memory save`, so a project's
/// embedding coverage climbs even without the dashboard's periodic auto-embed
/// daemon running. No-op (immediately, zero Ollama traffic) when embeddings
/// are disabled. Never fails the caller — `memory save` has already printed
/// its result by the time this runs.
fn try_opportunistic_embed_sweep(identity: &ProjectIdentity, timeout: std::time::Duration) {
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    if !cfg.embeddings.is_enabled() {
        return;
    }
    let identity = identity.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel::<usize>(1);
    std::thread::spawn(move || {
        let embedded = (|| -> Option<usize> {
            let store = MemoryStore::open_project(&identity).ok()?;
            let embedder = rtrt_memory::hybrid_embedder_from_config(&cfg);
            Some(store.opportunistic_embed_sweep(&embedder))
        })()
        .unwrap_or(0);
        let _ = tx.send(embedded);
    });
    let _ = rx.recv_timeout(timeout);
}

fn run_hook_recall(project: Option<String>, store: Option<PathBuf>, limit: usize) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok();
    // The prompt text is either the `prompt` field of a JSON payload or the
    // whole stdin when it isn't JSON.
    let prompt = extract_json_str(&raw, "prompt").unwrap_or_else(|| raw.trim().to_string());
    let (identity, project) = resolve_hook_project(project)?;
    reject_normal_store_override(store.as_deref())?;
    let store_path = rtrt_core::project_memory_db_path(&identity)?;
    let Some(hits) = recall_hits_for_hook(Some(&identity), &project, &store_path, &prompt, limit)
    else {
        return Ok(());
    };
    // stdout of a UserPromptSubmit hook is injected into the model context.
    println!("## Relevant project memory ({project})");
    let mut recalled_chars = 0u64;
    for h in hits {
        let body = h.body.replace('\n', " ");
        let clipped: String = body.chars().take(240).collect();
        recalled_chars = recalled_chars.saturating_add(clipped.chars().count() as u64);
        println!("- [{}] {}", h.kind, clipped);
    }
    // Recall reuse: this context is reused instead of being re-derived from
    // source, so log the recalled char count as a per-project Memory saving.
    record_recall_savings(&project, recalled_chars);
    Ok(())
}

/// Core auto-recall logic, factored out of [`run_hook_recall`] so it is
/// testable without stdin/stdout: given the already-extracted `prompt`,
/// returns the hits that should be injected, or `None` when nothing should.
///
/// **Relevance is decided by DENSE SIMILARITY, not by lexical statistics.**
/// The lexical leg answers "which rows share words with this prompt"; only
/// the embedding answers "is this row about what the prompt is about". The
/// gate is therefore the raw prompt-vs-row cosine, read against the
/// distribution of that same cosine over the project's whole corpus (see
/// [`rtrt_memory::SimilarityBackground`]): a hit is dense-supported when it
/// sits `√(2·ln n)` background standard deviations above the harsher of the
/// corpus mean and the prompt's MEANING-FREE TWIN
/// ([`rtrt_memory::null_probe_text`]). Nothing there is a tuned constant —
/// the sigma count comes from the corpus size, the sigma from the corpus
/// itself, and the twin from the prompt.
///
/// The twin is what makes the bar honest in both directions. Because it keeps
/// every surface property of the prompt and none of its meaning, a result set
/// that only matched on surface cannot clear it — which covers both "the
/// corpus holds nothing relevant" and "the embedding model cannot resolve
/// this prompt's language at all" (a model whose tokenizer does not cover a
/// script returns near-identical vectors for unrelated sentences of the same
/// shape, and would otherwise report that noise as a confident 0.9 match).
///
/// Order of operations:
///
/// 1. Sanitize the prompt into an FTS5 OR-join for the lexical leg.
/// 2. Recall: hybrid (BM25 lexical leg + raw-prompt dense-vector leg) when
///    safely available, else pure BM25 — never blocking beyond the hybrid
///    timeout (BUG2: the raw prompt, not the OR-join, is what gets embedded).
/// 3. PRIMARY GATE: keep the dense-supported hits. When at least one hit
///    clears the bar the dense leg has resolved the prompt and its verdict
///    stands on its own.
/// 4. FALLBACK GATE: when no hit clears the bar — no embedder, hybrid timed
///    out, corpus holds nothing relevant, or the model cannot resolve the
///    prompt — fall back to the conservative lexical floor
///    ([`rtrt_memory::MemoryStore::significant_terms`]): the prompt must own
///    at least one term statistically enriched in this project, and every
///    surfaced hit must contain one. No significant term means inject
///    nothing.
/// 5. Self-recall exclusion (BUG1): drop any hit that IS the very prompt row
///    `hook capture user-prompt-submit` just saved (same body hash, or the
///    same trimmed text as a belt-and-suspenders check).
/// 6. Legacy synthetic-noise filter (BUG4): drop any hit whose body is a
///    harness-injected event captured before PR #76 stopped saving them.
/// 7. Dedupe: the same text is captured many times over a project's life
///    (a repeated status line, a re-sent instruction), and both recall legs
///    happily return every copy. Inject each distinct body once.
fn recall_hits_for_hook(
    identity: Option<&ProjectIdentity>,
    project: &str,
    store_path: &Path,
    prompt: &str,
    limit: usize,
) -> Option<Vec<rtrt_memory::MemoryRecord>> {
    if prompt.trim().is_empty() || !store_path.exists() {
        return None;
    }
    let memory = match identity {
        Some(identity) => MemoryStore::open_project(identity).ok()?,
        #[cfg(test)]
        None => MemoryStore::open(store_path).ok()?,
        #[cfg(not(test))]
        None => return None,
    };
    // Build a safe FTS5 OR query via the shared sanitizer: a natural-language
    // prompt joined with spaces is treated as implicit AND by FTS5 (and its
    // punctuation can hard-fail the parser), while OR-joining the content
    // words ranks any row sharing a term — what context injection wants.
    // Stopwords and sub-3-char tokens are dropped to cut noise.
    let bm25_query = rtrt_memory::sanitize_fts_query(prompt)?;

    // Recall strategy: try HYBRID (BM25 + dense vector RRF) only when it can be
    // done safely, otherwise fall back to pure BM25 — and NEVER stall the
    // prompt. Hybrid is attempted iff:
    //   1. embeddings are enabled in config/env, AND
    //   2. the project already has meaningful embedding coverage
    //      (embedded > 0 && embedded*2 >= total) — without coverage hybrid
    //      adds latency for no recall gain, AND
    //   3. an OllamaEmbedder can be constructed.
    // The hybrid call runs on a detached worker thread bounded by a short join
    // timeout, so a slow/unreachable Ollama (ureq has no short default timeout)
    // falls back to BM25 fast. The BM25 leg gets the sanitized OR-join
    // (`bm25_query`); the dense-vector leg gets the RAW `prompt` (BUG2 fix —
    // the embedder needs real natural language, not a keyword bag).
    const HYBRID_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
    let recall = identity.and_then(|identity| {
        try_hybrid_recall(
            identity,
            project,
            &bm25_query,
            prompt,
            limit,
            HYBRID_TIMEOUT,
        )
    });

    // PRIMARY GATE (similarity). `recall_vector` ranks by cosine but returns
    // its top-K whatever those cosines are, so rank alone can never say "none
    // of these is relevant". The bar can.
    let dense: Vec<rtrt_memory::MemoryRecord> = recall
        .as_ref()
        .map(|r| {
            r.dense_supported()
                .into_iter()
                .map(|h| h.record.clone())
                .collect()
        })
        .unwrap_or_default();

    // FALLBACK GATE (lexical). Reached when the dense leg abstained: no
    // embedder / timeout, or nothing cleared the bar. Terms enriched in this
    // project are required both of the prompt (or there is no signal to
    // recall on) and of each hit (or the hit is not what the prompt asked
    // about).
    let (hits, lexical_floor) = if dense.is_empty() {
        let candidates = match recall {
            Some(r) => r.hits.into_iter().map(|h| h.record).collect(),
            None => memory
                .recall_bm25(project, &bm25_query, limit)
                .unwrap_or_default(),
        };
        let t_lex = std::time::Instant::now();
        let significant = memory
            .significant_terms(project, prompt)
            .unwrap_or_default();
        if std::env::var_os("RTRT_RECALL_DEBUG").is_some() {
            eprintln!(
                "rtrt recall debug: lexical fallback terms={significant:?} elapsed={:?}",
                t_lex.elapsed()
            );
        }
        if significant.is_empty() {
            return None;
        }
        (candidates, significant)
    } else {
        (dense, Vec::new())
    };

    // BUG1 fix (self-recall): `hook capture user-prompt-submit` runs before
    // `hook recall` and already saved this exact prompt as a memory row, so
    // drop any hit that IS that row — matched by body hash (primary) and by
    // trimmed-text equality (belt and suspenders, e.g. if the row predates a
    // hashing change). BUG4 fix: also drop legacy rows that are
    // harness-injected noise (task-id/tool-use-id dumps, <task-notification>
    // blocks) captured before PR #76 stopped saving them.
    let prompt_trimmed = prompt.trim();
    let prompt_sha = MemoryStore::body_sha(prompt_trimmed);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let hits: Vec<_> = hits
        .into_iter()
        .filter(|h| h.body.trim() != prompt_trimmed && MemoryStore::body_sha(&h.body) != prompt_sha)
        .filter(|h| !is_synthetic_prompt(&h.body))
        .filter(|h| {
            // Substring, not token, containment: agglutinative languages
            // attach particles to the stem, so a corpus token 토큰이 must
            // still count as carrying the query term 토큰.
            lexical_floor.is_empty() || {
                let body_lower = h.body.to_lowercase();
                lexical_floor
                    .iter()
                    .any(|t| body_lower.contains(t.as_str()))
            }
        })
        .filter(|h| seen.insert(MemoryStore::body_sha(h.body.trim())))
        .collect();

    if hits.is_empty() { None } else { Some(hits) }
}

fn recall_savings_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".rtrt").join("recall-savings.tsv"))
}

/// Append a recall-reuse sample (`project\tchars`). Each recall reuses stored
/// context the agent would otherwise re-derive, so the recalled char count is a
/// measured Memory saving.
fn record_recall_savings(project: &str, chars: u64) {
    if chars == 0 {
        return;
    }
    let Some(path) = recall_savings_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{project}\t{chars}");
    }
}

/// Total recall-reuse chars logged for a project.
fn read_recall_savings(project: &str) -> u64 {
    let Some(path) = recall_savings_path() else {
        return 0;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 0;
    };
    let mut total = 0u64;
    for line in content.lines() {
        let mut fields = line.split('\t');
        if let (Some(p), Some(c)) = (fields.next(), fields.next())
            && p == project
        {
            total = total.saturating_add(c.parse::<u64>().unwrap_or(0));
        }
    }
    total
}

/// SessionStart context injection. Prints the project's top-N memories
/// sorted by importance into stdout so Claude Code injects them into the
/// model context at the start of every session. No prompt is needed because
/// we surface the most salient background knowledge unconditionally.
fn run_hook_session_inject(
    project: Option<String>,
    store: Option<PathBuf>,
    limit: usize,
) -> Result<()> {
    let (identity, project) = resolve_hook_project(project)?;
    reject_normal_store_override(store.as_deref())?;
    let store_path = rtrt_core::project_memory_db_path(&identity)?;
    if !store_path.exists() {
        return Ok(());
    }
    let memory = MemoryStore::open_project(&identity)?;
    // Fetch the top memories ordered by importance (deterministic — recency +
    // length + compression + metadata bonuses). This surface is most useful at
    // session start because the agent hasn't asked anything yet.
    let rows = memory
        .recent_paged_by_importance(&project, limit, 0)
        .unwrap_or_default();
    if rows.is_empty() {
        return Ok(());
    }
    // stdout of a SessionStart hook is injected into the model context.
    println!("## Project memory ({project}) — top {} entries", rows.len());
    for r in rows {
        let body = r.body.replace('\n', " ");
        let clipped: String = body.chars().take(240).collect();
        println!("- [{}] {}", r.kind, clipped);
    }
    Ok(())
}

/// Read a Claude Code transcript JSONL and return the text of the most
/// recent assistant turn — the agent's own output for the turn that just
/// ended. Concatenates the `text` blocks of the last `type:"assistant"`
/// entry that has any (skipping pure tool-use turns). Returns None when the
/// path is empty/unreadable or no assistant text is found.
fn last_assistant_text(transcript_path: &str) -> Option<String> {
    if transcript_path.is_empty() {
        return None;
    }
    let content = std::fs::read_to_string(transcript_path).ok()?;
    // Walk lines bottom-up; first assistant entry with text wins.
    for line in content.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let blocks = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array());
        let Some(blocks) = blocks else { continue };
        let text: String = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    None
}

/// Pull a top-level string field out of a JSON object without a full
/// typed deserialize. Returns None when the input isn't an object or the
/// key is absent / non-string.
fn extract_json_str(raw: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.get(key)?.as_str().map(|s| s.to_string())
}

/// Turn a Claude Code hook payload into a concise, readable one-liner (or
/// short block) keyed off the event `kind`. The payloads are JSON objects
/// with an envelope (`session_id`, `cwd`, `hook_event_name`) plus
/// event-specific fields. When the body isn't JSON we keep the raw text so
/// nothing is silently lost.
///
/// Returns None to skip a capture entirely — used for events that carry no
/// useful signal (blank prompt, tool call with no input).
fn summarize_hook_payload(kind: &str, raw: &str) -> Option<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        // Not JSON — treat the whole thing as the body.
        return Some(raw.trim().to_string());
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let summary = match kind {
        "pre-tool-use" | "post-tool-use" | "post-tool-use-failure" => {
            let tool = get("tool_name");
            let input = v
                .get("tool_input")
                .map(compact_json_value)
                .unwrap_or_default();
            let result = if kind == "post-tool-use" {
                v.get("tool_response").map(|_| " → ok").unwrap_or("")
            } else if kind == "post-tool-use-failure" {
                " → failed"
            } else {
                ""
            };
            let head = if tool.is_empty() { kind } else { tool };
            format!("{head}: {input}{result}").trim().to_string()
        }
        "user-prompt-submit" | "user-prompt-expansion" => {
            let prompt = get("prompt").trim();
            // Claude Code delivers harness-injected events (background-task
            // notifications, `<task-notification>` blocks, bare task/tool-use
            // metadata) through the SAME UserPromptSubmit channel as real user
            // typing. Capturing those pollutes the user's own prompt history
            // (they are literally tagged "NOT USER INPUT"), so skip them.
            if is_synthetic_prompt(prompt) {
                return None;
            }
            prompt.to_string()
        }
        "notification" => get("message").trim().to_string(),
        "pre-compact" | "post-compact" => {
            let trigger = get("trigger");
            format!("compact ({kind}) trigger={trigger}")
                .trim()
                .to_string()
        }
        "session-start" => format!("session start: {}", get("source")),
        "session-end" => format!("session end: {}", get("reason")),
        // Stop / SubagentStop fire when the agent finishes a turn. The Stop
        // payload carries no content, but it does carry `transcript_path` —
        // so pull the agent's own last text response from the transcript.
        // This is what actually captures the agent's output (its reasoning,
        // decisions, summaries) into memory, which tool/prompt hooks miss.
        "stop" | "subagent-stop" => last_assistant_text(get("transcript_path"))?,
        // PostToolBatch carries a list of tool uses; surface the tool names
        // instead of a bare marker.
        "post-tool-batch" => {
            let names = v
                .get("tool_uses")
                .or_else(|| v.get("tools"))
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.get("tool_name").or_else(|| t.get("name")))
                        .filter_map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if names.is_empty() {
                return None; // nothing useful in the batch envelope
            }
            format!("tool batch: {names}")
        }
        // SubagentStart and anything else: terse marker, low value.
        _ => return None,
    };
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// Render a JSON value as a compact single-line string, clipped so a giant
/// tool input doesn't dominate the row. Strings are unquoted for brevity.
fn compact_json_value(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(200).collect()
}

fn reject_normal_store_override(store: Option<&Path>) -> Result<()> {
    if let Some(path) = store {
        bail!(
            "--store={} cannot select a normal memory store; use --admin-legacy-store explicitly",
            path.display()
        );
    }
    if let Some(path) = std::env::var_os("RTRT_MEMORY_PATH") {
        bail!(
            "RTRT_MEMORY_PATH={:?} cannot select a normal memory store; use --admin-legacy-store explicitly",
            path
        );
    }
    Ok(())
}

fn open_cli_memory(
    admin: Option<&Path>,
    deprecated_store: Option<&Path>,
    project_claim: Option<&str>,
) -> Result<(MemoryStore, String, Option<ProjectIdentity>, PathBuf)> {
    if let Some(admin_path) = admin {
        if let Some(path) = deprecated_store
            && path != admin_path
        {
            bail!("--store must match --admin-legacy-store in admin/legacy mode");
        }
        let project = project_claim
            .filter(|project| !project.trim().is_empty())
            .context("admin/legacy mode requires explicit --project")?
            .to_string();
        return Ok((
            MemoryStore::open(admin_path)?,
            project,
            None,
            admin_path.to_path_buf(),
        ));
    }
    reject_normal_store_override(deprecated_store)?;
    let identity = current_project_identity()?;
    let project = assert_current_project(&identity, project_claim)?;
    let path = rtrt_core::project_memory_db_path(&identity)?;
    let store = MemoryStore::open_project(&identity)?;
    Ok((store, project, Some(identity), path))
}

async fn run_memory(cmd: MemoryCmd, admin_legacy_store: Option<PathBuf>) -> Result<()> {
    let admin = admin_legacy_store.as_deref();
    match cmd {
        MemoryCmd::Save {
            project,
            kind,
            body,
            store: store_path,
            meta,
        } => {
            let (store, project, identity, _) =
                open_cli_memory(admin, store_path.as_deref(), project.as_deref())?;
            let body = read_body_or_stdin(body)?;
            let id = if meta.is_empty() {
                store.save(&project, &kind, &body)?
            } else {
                let map: std::collections::BTreeMap<String, String> = meta.into_iter().collect();
                store.save_with_metadata(&project, &kind, &body, &map)?
            };
            println!("saved id={id}");
            drop(store);
            // Grow embedding coverage opportunistically, bounded so it can't
            // meaningfully delay this command's exit; no-op when embeddings
            // are disabled.
            if let Some(identity) = identity {
                try_opportunistic_embed_sweep(&identity, OPPORTUNISTIC_EMBED_TIMEOUT);
            }
        }
        MemoryCmd::Blocks { cmd } => match cmd {
            BlockCmd::Set {
                project,
                name,
                body,
                store,
            } => {
                let (store, project, _, _) =
                    open_cli_memory(admin, store.as_deref(), project.as_deref())?;
                let body = read_body_or_stdin(body)?;
                let id = store.set_block(&project, &name, &body)?;
                println!("block id={id}");
            }
            BlockCmd::Get {
                project,
                name,
                store,
            } => {
                let (store, project, _, _) =
                    open_cli_memory(admin, store.as_deref(), project.as_deref())?;
                match store.get_block(&project, &name)? {
                    Some(b) => println!("{}", b.body),
                    None => anyhow::bail!("block not found: {name}"),
                }
            }
            BlockCmd::List { project, store } => {
                let (store, project, _, _) =
                    open_cli_memory(admin, store.as_deref(), project.as_deref())?;
                let blocks = store.list_blocks(&project)?;
                if blocks.is_empty() {
                    println!("(no blocks)");
                } else {
                    for b in blocks {
                        let name = b.kind.trim_start_matches("block:");
                        println!("- {name}: {}", b.body);
                    }
                }
            }
        },
        MemoryCmd::Recall {
            project,
            query,
            limit,
            store,
            filter,
        } => {
            let (store, project, _, _) =
                open_cli_memory(admin, store.as_deref(), project.as_deref())?;
            let hits = match filter {
                Some(spec) => {
                    let f = rtrt_memory::PayloadFilter::parse(&spec)?;
                    store.recall_bm25_with_filter(&project, &query, limit, &f)?
                }
                None => store.recall_bm25(&project, &query, limit)?,
            };
            for h in hits {
                println!("[{}] {} {}", h.id, h.kind, h.body);
            }
        }
        MemoryCmd::Export {
            project,
            store,
            out,
        } => {
            let (store, project, _, _) =
                open_cli_memory(admin, store.as_deref(), project.as_deref())?;
            let count = match out {
                Some(p) if p.as_os_str() != "-" => {
                    let f = std::fs::File::create(&p)?;
                    store.export_jsonl(&project, std::io::BufWriter::new(f))?
                }
                _ => {
                    let stdout = std::io::stdout();
                    store.export_jsonl(&project, stdout.lock())?
                }
            };
            eprintln!("[rtrt memory export] {count} records");
        }
        MemoryCmd::Import { store, input } => {
            let input: Box<dyn BufRead> = match input {
                Some(p) if p.as_os_str() != "-" => {
                    Box::new(std::io::BufReader::new(std::fs::File::open(&p)?))
                }
                _ => Box::new(std::io::BufReader::new(std::io::stdin())),
            };
            let count = if admin.is_some() {
                let (store, _, _, _) =
                    open_cli_memory(admin, store.as_deref(), Some("admin-import"))?;
                store.import_jsonl(input)?
            } else {
                reject_normal_store_override(store.as_deref())?;
                let identity = current_project_identity()?;
                let project = assert_current_project(&identity, None)?;
                let store = MemoryStore::open_project(&identity)?;
                import_project_jsonl(&store, &identity, &project, input)?
            };
            eprintln!("[rtrt memory import] {count} records");
        }
        MemoryCmd::Reembed {
            store: store_path,
            project,
            all,
            model,
            base_url,
            batch,
            workers,
            dry_run,
            probe,
        } => {
            let (store, pinned_project, _, _) = if admin.is_some() {
                let claim = project.as_deref().or(all.then_some("admin-all"));
                open_cli_memory(admin, store_path.as_deref(), claim)?
            } else {
                anyhow::ensure!(!all, "--all requires --admin-legacy-store");
                open_cli_memory(None, store_path.as_deref(), project.as_deref())?
            };
            let scope = if admin.is_some() && all {
                None
            } else {
                Some(pinned_project.as_str())
            };
            run_memory_reembed(
                store,
                scope,
                model.as_deref(),
                base_url.as_deref(),
                batch,
                workers,
                dry_run,
                probe,
            )?;
        }
        MemoryCmd::Extract {
            project,
            kind,
            body,
            provider,
            model,
            base_url,
            store,
        } => {
            let (store, project, _, _) =
                open_cli_memory(admin, store.as_deref(), project.as_deref())?;
            let body = read_body_or_stdin(body)?;
            let p = build_provider(provider, base_url, &model)?;
            let summariser = LlmSummariser::new(p, model);
            let ids = store
                .extract_and_save(&project, &kind, &body, &summariser)
                .await?;
            println!("extracted {} fact(s):", ids.len());
            for id in ids {
                println!("  id={id}");
            }
        }
        MemoryCmd::Compress {
            project,
            keep,
            provider,
            model,
            base_url,
            store,
        } => {
            let (store, project, _, _) =
                open_cli_memory(admin, store.as_deref(), project.as_deref())?;
            let p = build_provider(provider, base_url, &model)?;
            let summariser = LlmSummariser::new(p, model);
            match store.compress_project(&project, &summariser, keep).await? {
                Some(id) => println!("archival id={id}; older entries deleted"),
                None => println!("nothing to compress (have ≤ {keep} entries)"),
            }
        }
        MemoryCmd::LegacyIsolate {
            source,
            apply,
            claim_basename,
            accept_mixed_history,
        } => run_legacy_isolation_migration(&source, apply, claim_basename, accept_mixed_history)?,
    }
    Ok(())
}

fn import_project_jsonl(
    store: &MemoryStore,
    identity: &ProjectIdentity,
    project: &str,
    mut input: Box<dyn BufRead>,
) -> Result<usize> {
    let mut normalized = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let mut value: serde_json::Value = serde_json::from_str(line.trim())?;
        let asserted = value
            .get("project")
            .and_then(serde_json::Value::as_str)
            .context("jsonl: missing `project` assertion")?;
        if !project_claim_matches(identity, asserted) {
            bail!("foreign project assertion rejected in JSONL: {asserted:?}");
        }
        value["project"] = serde_json::Value::String(project.to_string());
        writeln!(&mut normalized, "{}", value)?;
    }
    store
        .import_jsonl(std::io::BufReader::new(normalized.as_slice()))
        .map_err(anyhow::Error::from)
}

#[derive(Debug)]
struct LegacyIsolationReport {
    total: usize,
    proven: usize,
    ambiguous: usize,
    copied: usize,
    already_copied: usize,
    skipped_unattributed: usize,
    quarantined_embeddings: usize,
    quarantined_relations: usize,
}

fn run_legacy_isolation_migration(
    source: &Path,
    apply: bool,
    claim_basename: bool,
    accept_mixed_history: bool,
) -> Result<()> {
    if claim_basename && !accept_mixed_history {
        bail!("--claim-basename requires --accept-mixed-history");
    }
    let identity = current_project_identity()?;
    let source = source
        .canonicalize()
        .with_context(|| format!("canonicalize legacy source {}", source.display()))?;
    let destination = rtrt_core::project_memory_db_path(&identity)?;
    anyhow::ensure!(
        source != destination,
        "legacy source is already current project destination"
    );

    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
        | rusqlite::OpenFlags::SQLITE_OPEN_URI;
    let source_db = rusqlite::Connection::open_with_flags(&source, flags)
        .with_context(|| format!("open legacy source read-only: {}", source.display()))?;
    let total: usize =
        source_db.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    let proven: usize = source_db.query_row(
        "SELECT COUNT(*) FROM memories WHERE project IN (?1, ?2, ?3)",
        rusqlite::params![
            identity.slug(),
            identity.fingerprint(),
            identity.memory_root().to_string_lossy()
        ],
        |row| row.get(0),
    )?;
    let ambiguous: usize = source_db.query_row(
        "SELECT COUNT(*) FROM memories WHERE project = ?1",
        [identity.label()],
        |row| row.get(0),
    )?;
    let selected_sql = if claim_basename {
        "project IN (?1, ?2, ?3, ?4)"
    } else {
        "project IN (?1, ?2, ?3)"
    };
    let selected_ids_sql = format!("SELECT id FROM memories WHERE {selected_sql}");
    let selected_params: Vec<String> = [
        identity.slug().to_string(),
        identity.fingerprint().to_string(),
        identity.memory_root().to_string_lossy().into_owned(),
    ]
    .into_iter()
    .chain(claim_basename.then(|| identity.label().to_string()))
    .collect();
    let selected_count = proven + if claim_basename { ambiguous } else { 0 };
    let skipped_unattributed = total.saturating_sub(selected_count);
    let embedding_sql =
        format!("SELECT COUNT(*) FROM embeddings WHERE memory_id IN ({selected_ids_sql})");
    let relation_sql = format!(
        "SELECT COUNT(*) FROM edges WHERE src_id IN ({selected_ids_sql}) OR dst_id IN ({selected_ids_sql})"
    );
    let refs = selected_params
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let quarantined_embeddings: usize = source_db.query_row(
        &embedding_sql,
        rusqlite::params_from_iter(refs.iter().copied()),
        |row| row.get(0),
    )?;
    let quarantined_relations: usize = source_db.query_row(
        &relation_sql,
        rusqlite::params_from_iter(refs.iter().copied()),
        |row| row.get(0),
    )?;

    let mut report = LegacyIsolationReport {
        total,
        proven,
        ambiguous,
        copied: 0,
        already_copied: 0,
        skipped_unattributed,
        quarantined_embeddings,
        quarantined_relations,
    };
    if apply && selected_count > 0 {
        // Creates and identity-binds only current project's destination.
        let _destination_guard = MemoryStore::open_project(&identity)?;
        let mut destination_db = rusqlite::Connection::open_with_flags(
            &destination,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let tx = destination_db.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS legacy_isolation_imports (
                source_path TEXT NOT NULL,
                source_id INTEGER NOT NULL,
                destination_id INTEGER NOT NULL,
                PRIMARY KEY(source_path, source_id)
             );",
        )?;
        let select_sql = format!(
            "SELECT id, kind, body, created_at, scope, metadata, session_id, body_sha, body_full
               FROM memories WHERE {selected_sql} ORDER BY id"
        );
        let mut select = source_db.prepare(&select_sql)?;
        let rows = select.query_map(rusqlite::params_from_iter(refs.iter().copied()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;
        for row in rows {
            let (
                source_id,
                kind,
                body,
                created_at,
                scope,
                metadata,
                session_id,
                body_sha,
                body_full,
            ) = row?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_isolation_imports WHERE source_path=?1 AND source_id=?2)",
                rusqlite::params![source.to_string_lossy(), source_id],
                |row| row.get(0),
            )?;
            if exists {
                report.already_copied += 1;
                continue;
            }
            tx.execute(
                "INSERT INTO memories(project,kind,body,created_at,scope,metadata,session_id,body_sha,body_full)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                rusqlite::params![identity.slug(), kind, body, created_at, scope, metadata, session_id, body_sha, body_full],
            )?;
            let destination_id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO memories_fts(rowid,body) VALUES (?1,?2)",
                rusqlite::params![destination_id, body],
            )?;
            tx.execute(
                "INSERT INTO legacy_isolation_imports(source_path,source_id,destination_id) VALUES (?1,?2,?3)",
                rusqlite::params![source.to_string_lossy(), source_id, destination_id],
            )?;
            report.copied += 1;
        }
        tx.commit()?;
    }
    println!(
        "legacy isolation {}: source={} destination={} total={} proven={} ambiguous_basename={} copied={} already_copied={} skipped_unattributed={} quarantined_embeddings={} quarantined_relations={}",
        if apply { "apply" } else { "dry-run" },
        source.display(),
        destination.display(),
        report.total,
        report.proven,
        report.ambiguous,
        report.copied,
        report.already_copied,
        report.skipped_unattributed,
        report.quarantined_embeddings,
        report.quarantined_relations,
    );
    if report.ambiguous > 0 && !claim_basename {
        println!(
            "ambiguous basename rows were not attributed; pass --claim-basename --accept-mixed-history to claim them"
        );
    }
    if claim_basename {
        println!("accepted warning: historical basename mixing cannot be disentangled");
    }
    println!("legacy source remains untouched and is the migration backup");
    Ok(())
}

fn read_body_or_stdin(body: Option<String>) -> Result<String> {
    match body.as_deref() {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf.trim().to_string())
        }
        Some(s) => Ok(s.to_string()),
    }
}

fn parse_positive_usize(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "must be a positive integer".to_string())?;
    if value == 0 {
        return Err("must be greater than zero".to_string());
    }
    Ok(value)
}

fn parse_worker_count(value: &str) -> std::result::Result<usize, String> {
    let value = parse_positive_usize(value)?;
    if value > 32 {
        return Err("must not exceed 32".to_string());
    }
    Ok(value)
}

fn parse_embed_batch_size(value: &str) -> std::result::Result<usize, String> {
    let value = parse_positive_usize(value)?;
    if value > 256 {
        return Err("must not exceed 256".to_string());
    }
    Ok(value)
}

fn store_reembedded_vector(
    store: &MemoryStore,
    memory_id: i64,
    model: &str,
    vector: &[f32],
    expected_dimension: &mut Option<usize>,
) -> Result<()> {
    let expected = if let Some(expected) = *expected_dimension {
        anyhow::ensure!(
            vector.len() == expected,
            "embedding dimension {} does not match stored `{model}` dimension {expected}",
            vector.len()
        );
        expected
    } else {
        *expected_dimension = Some(vector.len());
        vector.len()
    };
    store.store_embedding_replace_checked(memory_id, model, vector, expected)?;
    Ok(())
}

/// Implementation of `rtrt memory reembed` — see [`MemoryCmd::Reembed`].
///
/// Loads the configured (or `--model` overridden) ollama embedder, opens the
/// store read/write, and sweeps `[unembedded backlog ∪ stale-model rows]` in
/// bounded batches. Each batch is fetched in a single SQL query, embedded
/// through the configured embedder, and written back row-by-row with
/// [`MemoryStore::store_embedding_replace_checked`] (an `INSERT OR REPLACE`) so the
/// row's `embeddings.model` column is upgraded atomically — that's what keeps
/// a crash/resume safe: a half-upgraded row set never appears to recall paths
/// filtering on the active model.
///
/// The legacy-vs-new row selection is itself safe across runs: rows still on
/// `nomic-embed-text` match `e.model <> target`, get re-embedded as `bge-m3`,
/// and on the next pass no longer match, so they are skipped. Unembedded rows
/// match `e.model IS NULL` in the same query, so a fresh `rtrt memory save`
/// during the run also gets picked up without dropping coverage in the recall
/// filter.
#[allow(clippy::too_many_arguments)]
fn run_memory_reembed(
    store: MemoryStore,
    project: Option<&str>,
    model_override: Option<&str>,
    base_url_override: Option<&str>,
    batch: usize,
    workers: usize,
    dry_run: bool,
    probe: bool,
) -> Result<()> {
    anyhow::ensure!(batch > 0, "--batch must be greater than zero");
    anyhow::ensure!(workers > 0, "--workers must be greater than zero");
    let cfg = rtrt_core::Config::load()?;
    let base_url = match base_url_override {
        Some(u) => u.to_string(),
        None => cfg
            .embeddings
            .resolved_base_url(cfg.auto_compress.base_url.as_deref()),
    };
    let model = match model_override {
        Some(m) => m.to_string(),
        None => cfg.embeddings.effective_model(),
    };
    anyhow::ensure!(
        !model.trim().is_empty(),
        "embedding model must not be empty"
    );

    let persisted_dimension = store.embedding_dimension_for_model(&model)?;
    let pending_total = store.reembed_pending_count(&model, project)?;
    if pending_total == 0 {
        println!("[rtrt memory reembed] store is already on model `{model}` — nothing to do");
        return Ok(());
    }

    let target = match project {
        Some(project) => format!("project=`{project}`"),
        None => "all projects".to_string(),
    };
    println!(
        "[rtrt memory reembed] {pending_total} row(s) pending switch to `{model}` (target={target}){}",
        if dry_run { " [dry-run]" } else { "" },
    );

    if dry_run {
        // Show a sample of one batch of stale rows so the operator can sanity
        // check that the count is plausible (e.g. not 0 because of a wrongly
        // typed model name, or not 200k because of a half-broken JOIN).
        let sample = store.reembed_batch(&model, project, batch.min(8))?;
        for (id, p, body) in sample {
            let preview: String = body.chars().take(80).collect();
            println!("  id={id} project=`{p}` body={preview:?}");
        }
        return Ok(());
    }

    if probe {
        anyhow::bail!("stale rows still present; run without --probe to sweep to `{model}`");
    }

    let embedder = OllamaEmbedder::new(&base_url, model.clone());
    let model_active = embedder.model_name().to_string();
    let mut expected_dimension = persisted_dimension;
    let mut done = 0usize;
    let mut pass = 0usize;
    let t0 = std::time::Instant::now();
    loop {
        let fetch_limit = batch
            .checked_mul(workers)
            .context("--batch multiplied by --workers overflowed")?;
        let rows = store.reembed_batch(&model_active, project, fetch_limit)?;
        if rows.is_empty() {
            break;
        }

        // Only HTTP work is parallel. MemoryStore owns one rusqlite connection,
        // so vector writes stay deterministic and single-threaded below.
        let results = std::thread::scope(|scope| {
            let handles = rows
                .chunks(batch)
                .map(|chunk| {
                    scope.spawn(|| -> Result<Vec<Vec<f32>>> {
                        let texts = chunk
                            .iter()
                            .map(|(_, _, body)| truncate_for_embed(body).into_owned())
                            .collect::<Vec<_>>();
                        let borrowed = texts.iter().map(String::as_str).collect::<Vec<_>>();
                        Ok(embedder.embed(&borrowed)?)
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("Ollama embedding worker panicked"))?
                })
                .collect::<Vec<_>>()
        });

        let mut first_error = None;
        for (chunk, result) in rows.chunks(batch).zip(results) {
            match result {
                Ok(vectors) if vectors.len() == chunk.len() => {
                    for ((id, _, _), vector) in chunk.iter().zip(vectors) {
                        store_reembedded_vector(
                            &store,
                            *id,
                            &model_active,
                            &vector,
                            &mut expected_dimension,
                        )?;
                        done += 1;
                    }
                }
                Ok(vectors) => {
                    first_error.get_or_insert_with(|| {
                        anyhow::anyhow!(
                            "embedder returned {} vectors for {} rows",
                            vectors.len(),
                            chunk.len()
                        )
                    });
                }
                Err(batch_error) => {
                    // Isolate a failed modern batch (or legacy partial batch)
                    // to individual rows. Successful retries are persisted;
                    // only genuinely failing rows remain for the next run.
                    let mut retry_error = None;
                    for (id, _, body) in chunk {
                        let text = truncate_for_embed(body);
                        match embedder.embed_one(text.as_ref()) {
                            Ok(vector) => {
                                if let Err(error) = store_reembedded_vector(
                                    &store,
                                    *id,
                                    &model_active,
                                    &vector,
                                    &mut expected_dimension,
                                ) {
                                    retry_error.get_or_insert(error);
                                    break;
                                } else {
                                    done += 1;
                                }
                            }
                            Err(error) => {
                                retry_error.get_or_insert(anyhow::Error::from(error));
                                break;
                            }
                        }
                    }
                    if let Some(retry_error) = retry_error {
                        first_error.get_or_insert_with(|| {
                            batch_error
                                .context(format!("single-row retry also failed: {retry_error}"))
                        });
                    }
                }
            }
        }
        pass += 1;
        if let Some(error) = first_error {
            let remaining = store.reembed_pending_count(&model_active, project)?;
            anyhow::bail!(
                "reembed batch failed after persisting {done} row(s); {remaining} remain: {error}"
            );
        }
        // Reload the pending count every pass — the count moved during the
        // batch, so showing a fraction `done/pending_total` would lie about
        // progress when fresh rows got appended concurrently. The count itself
        // is a fast indexed `COUNT(*)+LEFT JOIN` against ~160k rows.
        let remaining = store.reembed_pending_count(&model_active, project)?;
        let elapsed = t0.elapsed().as_secs_f32();
        let rate = if elapsed > 0.0 {
            done as f32 / elapsed
        } else {
            0.0
        };
        eprintln!(
            "[reembed] pass={pass} embedded={done} remaining={remaining} elapsed={elapsed:.1}s rate={rate:.0}/s model=`{model_active}`"
        );
        std::io::Write::flush(&mut std::io::stderr())?;
    }

    println!(
        "[rtrt memory reembed] done: {done} row(s) re-embedded across {pass} batch(es) in {:.1}s; store now on `{model_active}`",
        t0.elapsed().as_secs_f32()
    );
    Ok(())
}
fn build_provider(
    kind: ProviderArg,
    base_url: Option<String>,
    _model: &str,
) -> Result<Box<dyn Provider>> {
    let provider: Box<dyn Provider> = match kind {
        ProviderArg::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
            Box::new(AnthropicProvider::new(key))
        }
        ProviderArg::Openai => {
            let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
            Box::new(OpenAIProvider::new(key))
        }
        ProviderArg::OpenaiCompat => {
            let url =
                base_url.ok_or_else(|| anyhow::anyhow!("--base-url required for openai-compat"))?;
            let mut p = OpenAICompatibleProvider::new("openai-compat", url);
            if let Ok(key) = std::env::var("RTRT_PROVIDER_API_KEY") {
                p = p.with_api_key(key);
            }
            Box::new(p)
        }
    };
    Ok(provider)
}

fn gateway_from_env_or_config(config_base_url: Option<&str>) -> rtrt_providers::Gateway {
    let gateway = rtrt_providers::Gateway::from_env();
    if std::env::var_os("RTRT_PROVIDER_BASE_URL").is_some()
        || std::env::var_os("RTRT_OPENAI_COMPAT_URL").is_some()
    {
        return gateway;
    }
    let Some(url) = config_base_url else {
        return gateway;
    };
    let mut provider = OpenAICompatibleProvider::new("openai-compat", url.to_string());
    if let Ok(key) = std::env::var("RTRT_OPENAI_COMPAT_API_KEY") {
        provider = provider.with_api_key(key);
    }
    gateway
        .register("openai-compat", Box::new(provider), [] as [&'static str; 0])
        .with_default_last()
}

/// Iterative directory walk. Yields every regular file under `root`. Skips
/// `target/`, `.git/`, `.priv-storage/`, and `node_modules/` to keep the map
/// focused on source.
fn walk_dir(root: &std::path::Path) -> impl Iterator<Item = PathBuf> + use<> {
    let mut stack: Vec<PathBuf> = Vec::new();
    if root.is_dir() {
        stack.push(root.to_path_buf());
    } else if root.is_file() {
        return WalkIter {
            stack: vec![root.to_path_buf()],
        };
    }
    WalkIter { stack }
}

struct WalkIter {
    stack: Vec<PathBuf>,
}

impl Iterator for WalkIter {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        while let Some(top) = self.stack.pop() {
            if top.is_file() {
                return Some(top);
            }
            if !top.is_dir() {
                continue;
            }
            let name = top
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            if matches!(
                name.as_str(),
                "target" | ".git" | ".priv-storage" | "node_modules" | "dist" | "build"
            ) {
                continue;
            }
            let entries = match std::fs::read_dir(&top) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                self.stack.push(entry.path());
            }
        }
        None
    }
}

fn default_history_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let home = PathBuf::from(home);
    [home.join(".zsh_history"), home.join(".bash_history")]
        .into_iter()
        .find(|p| p.exists())
}

fn detect_provider(model: &str) -> ProviderArg {
    if model.starts_with("claude-") {
        ProviderArg::Anthropic
    } else if model.starts_with("gpt-") || model.starts_with("o") {
        ProviderArg::Openai
    } else {
        ProviderArg::OpenaiCompat
    }
}

#[cfg(test)]
mod route_tests {
    use rtrt_providers::CapScope;

    use super::*;

    fn route_opts(target: Option<&str>, failover: bool) -> RouteCliOptions {
        RouteCliOptions {
            capability: Some(RouteCapabilityArg::Code),
            prefer: RoutePreferArg::Cheapest,
            target: target.map(str::to_string),
            model: None,
            mode: CallModeArg::Auto,
            explain: false,
            dry_run: false,
            failover,
            prompt: vec!["ship it".to_string()],
        }
    }

    /// `opencode` reaching two upstream pools, plus a second target behind it.
    fn pooled_tools() -> Vec<DetectedTool> {
        let opencode = DetectedTool {
            name: "opencode".to_string(),
            kind: ToolKind::CodingAgent,
            installed: true,
            path: None,
            version: None,
            invocation_modes: vec![InvocationMode::Cli],
            cli_invocation: Some("opencode run {prompt}".to_string()),
            cost_class: CostClass::SubscriptionFlat,
            capabilities: vec![Capability::Code],
            config_path: None,
            models: vec![
                "opencode-go/glm-5.2".to_string(),
                "ollama/glm-5.2:cloud".to_string(),
            ],
            server_running: None,
            enabled: true,
        };
        let claude = DetectedTool {
            name: "claude".to_string(),
            cli_invocation: Some("claude -p {prompt}".to_string()),
            models: vec!["sonnet".to_string()],
            ..opencode.clone()
        };
        vec![opencode, claude]
    }

    /// The regression: `[providers] active` pinned every route, an explicit
    /// target collapsed the candidate list to one entry, and `--failover` had
    /// nothing to fall over to. Merely configuring `active` disarmed the flag.
    #[test]
    fn configured_active_target_no_longer_neuters_failover() {
        let tools = pooled_tools();
        let usage = UsageSnapshot::default();

        let with_failover = route_request_for(&route_opts(None, true), Some("opencode"));
        assert_eq!(with_failover.target.as_deref(), Some("opencode"));
        assert!(with_failover.failover);
        let decision = select_route(&with_failover, &tools, &usage).expect("failover route");
        assert_eq!(decision.target, "opencode");
        assert!(
            decision.ranked_targets().len() >= 2,
            "pinned + --failover must keep a tail: {decision:?}"
        );

        // Default behaviour is untouched: one pinned candidate, no tail.
        let pinned = route_request_for(&route_opts(None, false), Some("opencode"));
        assert!(!pinned.failover);
        let decision = select_route(&pinned, &tools, &usage).expect("pinned route");
        assert_eq!(decision.target, "opencode");
        assert!(decision.alternatives.is_empty(), "{decision:?}");
        assert_eq!(decision.ranked_targets().len(), 1);
    }

    #[test]
    fn explicit_target_still_wins_over_the_configured_active_target() {
        let req = route_request_for(&route_opts(Some("claude"), true), Some("opencode"));
        assert_eq!(req.target.as_deref(), Some("claude"));
        // No `--target` and no `[providers] active`: the route stays open.
        assert_eq!(
            route_request_for(&route_opts(None, false), None).target,
            None
        );
    }

    /// A usage-derived order must never read as a quota measurement.
    #[test]
    fn pool_basis_suffix_only_discloses_usage_derived_order() {
        assert_eq!(
            pool_basis_suffix(RoomBasis::ObservedUsage),
            " [pool order: usage-derived (observed 24h usage, not a quota measurement)]"
        );
        assert!(pool_basis_suffix(RoomBasis::Quota).is_empty());
    }

    #[test]
    fn pool_room_never_invents_a_ceiling_and_marks_shared_caps() {
        let uncapped = pool_room(
            PoolCap {
                scope: CapScope::Unknown,
                limit: None,
                used: 900,
                remaining: None,
            },
            PoolCap {
                scope: CapScope::Unknown,
                limit: None,
                used: 3,
                remaining: None,
            },
        );
        assert_eq!(format_pool_room(&uncapped), "? (no [limits] cap)");

        let shared = pool_room(
            PoolCap {
                scope: CapScope::Shared,
                limit: Some(1000),
                used: 900,
                remaining: Some(100),
            },
            PoolCap {
                scope: CapScope::Unknown,
                limit: None,
                used: 3,
                remaining: None,
            },
        );
        assert_eq!(
            format_pool_room(&shared),
            "100/1000 tok left (used 900) (cap shared with sibling pools)"
        );

        let own = pool_room(
            PoolCap {
                scope: CapScope::Pool,
                limit: Some(1000),
                used: 250,
                remaining: Some(750),
            },
            PoolCap {
                scope: CapScope::Pool,
                limit: Some(50),
                used: 5,
                remaining: Some(45),
            },
        );
        assert_eq!(
            format_pool_room(&own),
            "750/1000 tok left (used 250), 45/50 req left (used 5)"
        );
    }

    fn pool_room(tokens: PoolCap, requests: PoolCap) -> PoolHeadroom {
        PoolHeadroom {
            key: "opencode#opencode-go".to_string(),
            target: "opencode".to_string(),
            pool: Some("opencode-go".to_string()),
            used_tokens: tokens.used,
            used_requests: requests.used,
            tokens_estimated: false,
            tokens,
            requests,
            sibling_pools: 2,
        }
    }
}

#[cfg(test)]
mod hook_capture_tests {
    use super::*;

    #[test]
    fn synthetic_prompts_are_skipped() {
        // Harness-injected events that arrive through the UserPromptSubmit channel.
        assert!(is_synthetic_prompt(
            "[SYSTEM NOTIFICATION - NOT USER INPUT]\nThis is an automated background-task event"
        ));
        assert!(is_synthetic_prompt(
            "<task-notification>\n<task-id>bxyz</task-id>\n</task-notification>"
        ));
        assert!(is_synthetic_prompt(
            "task-id: bs2ne03kz, tool-use-id: toolu_01QyNuv9, output-file: /tmp/x.output"
        ));
        assert!(is_synthetic_prompt(
            "task-id:a5c726cce05,tool-use-id:toolu_01BpPZ,output-file:/tmp/y"
        ));
    }

    #[test]
    fn real_prompts_are_kept() {
        assert!(!is_synthetic_prompt(
            "근데 내가 입력한 항목은 왜 안떠 메모리에?"
        ));
        assert!(!is_synthetic_prompt("지금 왜 서비스 죽어있어?"));
        assert!(!is_synthetic_prompt(
            "fix the task-id parsing in the ledger" // mentions task-id but is real typing
        ));
        assert!(!is_synthetic_prompt("進行시켜"));
    }

    #[test]
    fn user_prompt_payload_skips_synthetic_and_keeps_real() {
        let synthetic = r#"{"prompt":"[SYSTEM NOTIFICATION - NOT USER INPUT]\nThis is an automated background-task event"}"#;
        assert_eq!(
            summarize_hook_payload("user-prompt-submit", synthetic),
            None
        );
        let real = r#"{"prompt":"왜 서비스 죽어있어?"}"#;
        assert_eq!(
            summarize_hook_payload("user-prompt-submit", real).as_deref(),
            Some("왜 서비스 죽어있어?")
        );
    }
}

#[cfg(test)]
mod hook_recall_tests {
    use super::*;

    /// BUG1 regression: `hook capture user-prompt-submit` runs before `hook
    /// recall` and already saved the current prompt as a memory row, so
    /// recall must not echo it back as one of its own hits — while a
    /// genuinely distinct, topically-relevant row still surfaces.
    #[test]
    fn recall_excludes_the_self_recalled_current_prompt_row() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.sqlite");
        let memory = MemoryStore::open(&store_path).unwrap();
        let prompt = "how does the gateway router headroom failover work";
        memory.save("proj", "user-prompt-submit", prompt).unwrap();
        memory
            .save(
                "proj",
                "note",
                "gateway router headroom failover design notes",
            )
            .unwrap();
        drop(memory);

        let hits =
            recall_hits_for_hook(None, "proj", &store_path, prompt, 5).expect("expected hits");
        assert!(
            hits.iter().all(|h| h.body.trim() != prompt.trim()),
            "self-recall row must be excluded: {hits:?}"
        );
        assert!(
            hits.iter().any(|h| h.body.contains("design notes")),
            "a genuinely relevant, distinct row must still surface: {hits:?}"
        );
    }

    /// BUG3 regression: the relevance floor must reject a prompt that shares
    /// no distinctive term with the project's corpus (zero hits), while a
    /// topical prompt naming a rare, project-specific term still recalls.
    #[test]
    fn recall_relevance_floor_suppresses_unrelated_prompts_but_not_topical_ones() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.sqlite");
        let memory = MemoryStore::open(&store_path).unwrap();
        // Corpus saturated with a generic word across many rows, plus one row
        // naming a rare, distinctive technical term.
        for i in 0..20 {
            memory
                .save("proj", "note", &format!("project status update {i}"))
                .unwrap();
        }
        memory
            .save(
                "proj",
                "note",
                "gateway router headroom failover design notes",
            )
            .unwrap();
        drop(memory);

        // Unrelated small talk: no term overlaps this project's corpus at all.
        let unrelated = recall_hits_for_hook(
            None,
            "proj",
            &store_path,
            "what should we eat for lunch today pasta kimchi stew",
            5,
        );
        assert!(unrelated.is_none(), "{unrelated:?}");

        // Topical query naming the rare technical term.
        let topical = recall_hits_for_hook(
            None,
            "proj",
            &store_path,
            "how does the gateway router headroom failover work",
            5,
        )
        .expect("expected hits for a topical query");
        assert!(
            topical.iter().any(|h| h.body.contains("failover")),
            "{topical:?}"
        );
    }

    /// On the lexical fallback path a candidate that matched the BM25
    /// OR-query only through a corpus-saturated term must be dropped even
    /// though the query as a whole cleared the floor via a DIFFERENT,
    /// genuinely project-specific term. Without the per-hit check the
    /// fallback would surface every row sharing any word with the prompt.
    #[test]
    fn recall_drops_hits_that_share_no_significant_term_with_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.sqlite");
        let memory = MemoryStore::open(&store_path).unwrap();
        for i in 0..20 {
            memory
                .save("proj", "note", &format!("status update {i}"))
                .unwrap();
        }
        // Matches the query only via the corpus-saturated word "update" — no
        // distinctive term in common with the query.
        memory
            .save(
                "proj",
                "note",
                "weekly status update, completely unconnected topic",
            )
            .unwrap();
        // Matches via the rare, distinctive term "failover" (and friends).
        memory
            .save(
                "proj",
                "note",
                "gateway router headroom failover design notes",
            )
            .unwrap();
        drop(memory);

        let hits = recall_hits_for_hook(
            None,
            "proj",
            &store_path,
            "gateway router headroom failover status update",
            10,
        )
        .expect("expected hits");
        assert!(hits.iter().all(|h| h.body.contains("failover")), "{hits:?}");
        assert!(
            hits.iter().all(|h| !h.body.contains("unconnected topic")),
            "{hits:?}"
        );
    }

    /// The same text is captured over and over across a project's life, so
    /// both recall legs return several rows with an identical body. Injecting
    /// the same sentence twice in one response wastes the context it was
    /// supposed to save.
    #[test]
    fn recall_dedupes_identical_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.sqlite");
        let memory = MemoryStore::open(&store_path).unwrap();
        for i in 0..20 {
            memory
                .save("proj", "note", &format!("project status update {i}"))
                .unwrap();
        }
        // The same observation captured three separate times.
        for _ in 0..3 {
            memory
                .save("proj", "note", "gateway headroom failover check failed")
                .unwrap();
        }
        drop(memory);

        let hits = recall_hits_for_hook(None, "proj", &store_path, "gateway headroom failover", 10)
            .expect("expected hits");
        let repeated = hits
            .iter()
            .filter(|h| h.body.contains("failover check failed"))
            .count();
        assert_eq!(
            repeated, 1,
            "duplicate bodies must be injected once: {hits:?}"
        );
    }

    /// BM25-fallback regression (no embedder available): the conservative
    /// lexical floor must reject a prompt whose only corpus footprint is the
    /// conversation that just introduced it — the ambient transcript watcher
    /// saves the user's own prompt, so an off-topic word ALWAYS exists in the
    /// corpus at a handful of rows by the time recall runs. A floor that
    /// rewards rarity promotes exactly that noise; enrichment against the
    /// rest of the store does not.
    #[test]
    fn recall_fallback_floor_rejects_self_contaminated_off_topic_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.sqlite");
        let memory = MemoryStore::open(&store_path).unwrap();
        // Cross-project background: without it there is no baseline to test
        // enrichment against.
        for i in 0..400 {
            memory
                .save("other", "note", &format!("unrelated journal entry {i}"))
                .unwrap();
        }
        // The project's own vocabulary — common HERE, rare everywhere else.
        for i in 0..120 {
            memory
                .save("proj", "note", &format!("headroom budget review {i}"))
                .unwrap();
        }
        // Three rows of one off-topic conversation, captured verbatim.
        for i in 0..3 {
            memory
                .save(
                    "proj",
                    "user-prompt-submit",
                    &format!("volcano question {i}"),
                )
                .unwrap();
        }
        drop(memory);

        let unrelated =
            recall_hits_for_hook(None, "proj", &store_path, "volcano eruption cause", 5);
        assert!(
            unrelated.is_none(),
            "a word this project only saw in one captured conversation is not signal: {unrelated:?}"
        );

        let topical =
            recall_hits_for_hook(None, "proj", &store_path, "how is headroom budgeted", 5)
                .expect("project vocabulary must still recall");
        assert!(
            topical.iter().all(|h| h.body.contains("headroom")),
            "{topical:?}"
        );
    }

    /// BUG4 regression: legacy `user-prompt-submit` rows captured before PR
    /// #76 stopped saving harness-injected noise must not surface in recall,
    /// even when they happen to token-match the query.
    #[test]
    fn recall_filters_legacy_synthetic_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("memory.sqlite");
        let memory = MemoryStore::open(&store_path).unwrap();
        memory
            .save(
                "proj",
                "user-prompt-submit",
                "task-id: xyz111, tool-use-id: toolu_01, output-file: /tmp/x note gateway router headroom failover",
            )
            .unwrap();
        memory
            .save(
                "proj",
                "note",
                "gateway router headroom failover design notes",
            )
            .unwrap();
        drop(memory);

        let hits = recall_hits_for_hook(
            None,
            "proj",
            &store_path,
            "how does the gateway router headroom failover work",
            5,
        )
        .expect("expected hits");
        assert!(
            hits.iter().all(|h| !h.body.starts_with("task-id:")),
            "{hits:?}"
        );
        assert!(
            hits.iter().any(|h| h.body.contains("design notes")),
            "{hits:?}"
        );
    }
}

#[cfg(test)]
mod statusline_tests {
    use super::*;

    fn segment_map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn opencode_facts() -> OpenCodeRenderFacts {
        OpenCodeRenderFacts {
            rendered_at: 10_000,
            project: "rtrt".into(),
            style: OutputStyleLevel::Full,
            savings: Some(OpenCodeSavings {
                sigma_pct: 42,
                command_pct: Some(35),
                saved_chars: Some(42),
                base_chars: Some(100),
                recall_chars: Some(7),
                cached: false,
            }),
            headroom: Some(OpenCodeHeadroom {
                text: "room:opencode:73%".into(),
                tone: "good",
            }),
            quota: None,
            git: Some(OpenCodeGitSnapshot {
                branch: "main".into(),
                dirty: true,
                cached: true,
            }),
            session: Some("abc123".into()),
            model: Some("gpt-5.4".into()),
            memory: Some(OpenCodeMemoryAggregate {
                rows: 12,
                original_chars: 100,
                stored_chars: 60,
            }),
        }
    }

    #[test]
    fn parses_opencode_statusline_options() {
        let cli = Cli::try_parse_from([
            "rtrt",
            "statusline",
            "--opencode",
            "--cwd",
            "/work/repo",
            "--session",
            "session-1",
            "--model",
            "openai/gpt-5.4",
            "--width",
            "88",
            "--budget-ms",
            "17",
            "--no-git",
            "--refresh",
        ])
        .unwrap();

        let Some(Cmd::Statusline {
            opencode,
            cwd,
            session,
            model,
            width,
            budget_ms,
            no_git,
            refresh,
            ..
        }) = cli.command
        else {
            panic!("expected statusline command");
        };
        assert!(opencode);
        assert_eq!(cwd, Some(PathBuf::from("/work/repo")));
        assert_eq!(session.as_deref(), Some("session-1"));
        assert_eq!(model.as_deref(), Some("openai/gpt-5.4"));
        assert_eq!(width, 88);
        assert_eq!(budget_ms, 17);
        assert!(no_git);
        assert!(refresh);
    }

    #[test]
    fn opencode_width_tiers_match_golden_segments() {
        let facts = opencode_facts();

        assert_eq!(
            serde_json::Value::Array(render_opencode_segments(40, &facts)),
            serde_json::json!([
                {"id":"sigma","text":"Σ:42%","tone":"good","pri":100},
                {"id":"style","text":"opt:full","tone":"accent","pri":90}
            ])
        );
        assert_eq!(
            serde_json::Value::Array(render_opencode_segments(80, &facts)),
            serde_json::json!([
                {"id":"project","text":"rtrt","tone":"accent","pri":100},
                {"id":"style","text":"opt:full","tone":"accent","pri":90},
                {"id":"savings","text":"save:42% cmd:35%","tone":"good","pri":80},
                {"id":"headroom","text":"room:opencode:73%","tone":"good","pri":70}
            ])
        );
        assert_eq!(
            serde_json::Value::Array(render_opencode_segments(100, &facts)),
            serde_json::json!([
                {"id":"project","text":"rtrt","tone":"accent","pri":100},
                {"id":"style","text":"opt:full","tone":"accent","pri":90},
                {"id":"savings","text":"save:42% cmd:35%","tone":"good","pri":80},
                {"id":"headroom","text":"room:opencode:73%","tone":"good","pri":70},
                {"id":"git","text":"main*","tone":"warn","pri":65},
                {"id":"model","text":"gpt-5.4","tone":"muted","pri":60},
                {"id":"session","text":"sess:abc123","tone":"muted","pri":50},
                {"id":"memory","text":"mem:12 40%","tone":"good","pri":40}
            ])
        );
    }

    fn rate_limit_cache_fixture(
        captured_at: u64,
        five_hour: Option<(f64, u64)>,
        seven_day: Option<(f64, u64)>,
    ) -> String {
        let mut windows = serde_json::Map::new();
        if let Some((used_percentage, resets_at)) = five_hour {
            windows.insert(
                "five_hour".into(),
                serde_json::json!({
                    "used_percentage": used_percentage,
                    "resets_at": resets_at,
                }),
            );
        }
        if let Some((used_percentage, resets_at)) = seven_day {
            windows.insert(
                "seven_day".into(),
                serde_json::json!({
                    "used_percentage": used_percentage,
                    "resets_at": resets_at,
                }),
            );
        }
        serde_json::json!({
            "v": 1,
            "captured_at": captured_at,
            "windows": windows,
        })
        .to_string()
    }

    #[test]
    fn claude_rate_limit_cache_accepts_partial_windows_and_filters_expired_ones() {
        let partial = rate_limit_cache_fixture(9_900, Some((23.5, 11_800)), None);
        let parsed = parse_claude_rate_limit_cache(&partial, 10_000, 900).unwrap();
        assert_eq!(parsed.captured_at, 9_900);
        assert_eq!(parsed.five_hour.unwrap().used_percentage, 23.5);
        assert!(parsed.seven_day.is_none());

        let one_expired =
            rate_limit_cache_fixture(9_900, Some((80.0, 10_000)), Some((40.0, 20_000)));
        let parsed = parse_claude_rate_limit_cache(&one_expired, 10_000, 900).unwrap();
        assert!(parsed.five_hour.is_none());
        assert_eq!(parsed.seven_day.unwrap().used_percentage, 40.0);

        let expired = rate_limit_cache_fixture(9_900, Some((80.0, 9_999)), None);
        assert!(parse_claude_rate_limit_cache(&expired, 10_000, 900).is_none());
    }

    #[test]
    fn claude_rate_limit_cache_rejects_malformed_stale_and_future_observations() {
        let fresh = rate_limit_cache_fixture(9_100, Some((20.0, 11_000)), None);
        assert!(parse_claude_rate_limit_cache(&fresh, 10_000, 900).is_some());
        assert!(parse_claude_rate_limit_cache(&fresh, 10_001, 900).is_none());

        let future = rate_limit_cache_fixture(10_001, Some((20.0, 11_000)), None);
        assert!(parse_claude_rate_limit_cache(&future, 10_000, 900).is_none());

        let extra_field = r#"{"v":1,"captured_at":9900,"windows":{"five_hour":{"used_percentage":20,"resets_at":11000}},"session_id":"secret"}"#;
        assert!(parse_claude_rate_limit_cache(extra_field, 10_000, 900).is_none());

        let invalid_number = rate_limit_cache_fixture(9_900, Some((100.1, 11_000)), None);
        assert!(parse_claude_rate_limit_cache(&invalid_number, 10_000, 900).is_none());

        let control = format!(
            "{}\n",
            rate_limit_cache_fixture(9_900, Some((20.0, 11_000)), None)
        );
        assert!(parse_claude_rate_limit_cache(&control, 10_000, 900).is_none());
    }

    #[test]
    fn claude_rate_limit_max_age_override_is_bounded() {
        assert_eq!(bounded_claude_rate_limit_max_age(None), 15 * 60);
        assert_eq!(bounded_claude_rate_limit_max_age(Some("1")), 1);
        assert_eq!(bounded_claude_rate_limit_max_age(Some("3600")), 3_600);
        assert_eq!(bounded_claude_rate_limit_max_age(Some("86401")), 15 * 60);
        assert_eq!(
            bounded_claude_rate_limit_max_age(Some("not-a-number")),
            15 * 60
        );
    }

    #[test]
    fn opencode_quota_segments_have_deterministic_reset_text_and_tones() {
        let good = opencode_quota_segment(
            "limit_5h",
            "5h",
            &ClaudeRateLimitWindow {
                used_percentage: 69.4,
                resets_at: 13_660,
            },
            10_000,
            30,
            75,
        );
        assert_eq!(good["text"], "5h:69% ↻1h1m");
        assert_eq!(good["tone"], "good");
        assert_eq!(good["source"], "claude_statusline");
        assert_eq!(good["freshness_sec"], 30);

        let rounded_warn = opencode_quota_segment(
            "limit_5h",
            "5h",
            &ClaudeRateLimitWindow {
                used_percentage: 69.5,
                resets_at: 13_660,
            },
            10_000,
            30,
            75,
        );
        assert_eq!(rounded_warn["text"], "5h:70% ↻1h1m");
        assert_eq!(rounded_warn["tone"], "warn");

        let warn = opencode_quota_segment(
            "limit_week",
            "wk",
            &ClaudeRateLimitWindow {
                used_percentage: 70.0,
                resets_at: 183_700,
            },
            10_000,
            30,
            74,
        );
        assert_eq!(warn["text"], "wk:70% ↻2d0h");
        assert_eq!(warn["tone"], "warn");

        let bad = opencode_quota_segment(
            "limit_week",
            "wk",
            &ClaudeRateLimitWindow {
                used_percentage: 90.0,
                resets_at: 10_060,
            },
            10_000,
            30,
            74,
        );
        assert_eq!(bad["tone"], "bad");
    }

    #[cfg(unix)]
    #[test]
    fn claude_rate_limit_cache_is_private_and_never_follows_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let fixture = tempfile::tempdir().unwrap();
        let parent = fixture.path().join("private");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cache = parent.join("claude-rate-limits.json");
        let victim = fixture.path().join("victim");
        std::fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, &cache).unwrap();
        let limits = ClaudeRateLimits {
            captured_at: 10_000,
            five_hour: Some(ClaudeRateLimitWindow {
                used_percentage: 20.0,
                resets_at: 11_000,
            }),
            seven_day: None,
        };

        write_claude_rate_limit_cache(&cache, &limits).unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "unchanged");
        let cache_metadata = std::fs::symlink_metadata(&cache).unwrap();
        assert!(cache_metadata.is_file());
        assert!(!cache_metadata.file_type().is_symlink());
        assert_eq!(cache_metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(
            std::fs::symlink_metadata(&parent)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        let raw = read_private_rate_limit_cache(
            &cache,
            CLAUDE_RATE_LIMIT_CACHE_MAX_BYTES,
            RateLimitCacheParentPolicy::Private,
        )
        .unwrap();
        assert!(parse_claude_rate_limit_cache(&raw, 10_000, 900).is_some());

        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_private_rate_limit_cache(
                &cache,
                CLAUDE_RATE_LIMIT_CACHE_MAX_BYTES,
                RateLimitCacheParentPolicy::Private,
            )
            .is_none()
        );
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            read_private_rate_limit_cache(
                &cache,
                CLAUDE_RATE_LIMIT_CACHE_MAX_BYTES,
                RateLimitCacheParentPolicy::Private,
            )
            .is_none()
        );

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&cache).unwrap();
        symlink(&victim, &cache).unwrap();
        assert!(
            read_private_rate_limit_cache(
                &cache,
                CLAUDE_RATE_LIMIT_CACHE_MAX_BYTES,
                RateLimitCacheParentPolicy::Private,
            )
            .is_none()
        );
    }

    #[test]
    fn renders_statusline_templates_with_injected_segments() {
        let cfg = StatuslineConfig::default();
        let segments = segment_map(&[
            ("project", "00G_rtrt"),
            ("branch", "feature/orchestrator-polish"),
            ("wip", "wip:1"),
            ("sess", "sess:1"),
            ("ctx", "ctx:74%(740k/1.0M)"),
            ("cache", "cache:97%"),
            ("opt", "opt:full"),
            ("model", "Opus 4.8"),
            ("usage", "5h:8% ↻52m | wk:28% ↻5d17h"),
            ("agents", "🤖 claude·codex"),
            ("savings", "💯Σ:0"),
        ]);

        assert_eq!(
            render_statusline(&cfg, &segments),
            "00G_rtrt [feature/orchestrator-polish] wip:1 sess:1 ctx:74%(740k/1.0M) cache:97% opt:full Opus 4.8 🤖 claude·codex\n5h:8% ↻52m | wk:28% ↻5d17h\n💯Σ:0"
        );
    }

    #[test]
    fn disabled_segments_are_absent_from_output() {
        let cfg = StatuslineConfig {
            enabled_segments: vec!["project".into(), "model".into(), "savings".into()],
            format: DEFAULT_STATUSLINE_FORMAT.to_string(),
            line2_format: DEFAULT_STATUSLINE_LINE2_FORMAT.to_string(),
            line3_format: DEFAULT_STATUSLINE_LINE3_FORMAT.to_string(),
            codex_check_timeout_ms: DEFAULT_CODEX_CHECK_TIMEOUT_MS,
        };
        let segments = segment_map(&[
            ("project", "00G_rtrt"),
            ("branch", "feature/orchestrator-polish"),
            ("wip", "wip:1"),
            ("sess", "sess:1"),
            ("ctx", "ctx:74%(740k/1.0M)"),
            ("opt", "opt:full"),
            ("model", "Opus 4.8"),
            ("agents", "🤖 claude·codex"),
            ("savings", "💯Σ:0"),
        ]);
        let rendered = render_statusline(&cfg, &segments);

        assert!(rendered.contains("00G_rtrt"));
        assert!(rendered.contains("Opus 4.8"));
        assert!(rendered.contains("💯Σ:0"));
        assert!(!rendered.contains("feature/orchestrator-polish"));
        assert!(!rendered.contains("wip:1"));
        assert!(!rendered.contains("sess:1"));
        assert!(!rendered.contains("ctx:"));
        assert!(!rendered.contains("opt:"));
        assert!(!rendered.contains("🤖 claude"));
    }

    #[test]
    fn parses_inline_statusline_toml_table() {
        let raw = r#"
            [other]
            format = "ignored"

            [statusline]
            enabled_segments = ["project", "branch", "model"]
            format = "{project}:{branch}"
            line2_format = "{model}"
            line3_format = ""
            codex_check_timeout_ms = 75
        "#;

        let cfg = parse_statusline_config(raw).expect("statusline config");

        assert_eq!(cfg.enabled_segments, ["project", "branch", "model"]);
        assert_eq!(cfg.format, "{project}:{branch}");
        assert_eq!(cfg.line2_format, "{model}");
        assert_eq!(cfg.line3_format, "");
        assert_eq!(cfg.codex_check_timeout_ms, 75);
    }

    #[test]
    fn formats_agents_segment_with_width_budget() {
        let names = ["claude", "codex", "opencode", "ollama"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();

        assert_eq!(format_agents_segment(&names, 24), "🤖 claude·codex·+2");
    }

    #[test]
    fn agents_cache_is_located_in_runtime_scratch_directory() {
        let runtime_tmp = Path::new("project").join(".rtrt").join("tmp");

        assert_eq!(
            agents_status_cache_path_in(&runtime_tmp, 0x2a),
            runtime_tmp.join("rtrt-agents-status-000000000000002a.cache")
        );
    }

    #[cfg(unix)]
    #[test]
    fn agents_cache_write_replaces_symlink_without_following_it() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let fixture = tempfile::tempdir().unwrap();
        let runtime_tmp = fixture.path().join("runtime");
        std::fs::create_dir(&runtime_tmp).unwrap();
        std::fs::set_permissions(&runtime_tmp, std::fs::Permissions::from_mode(0o700)).unwrap();
        let victim = fixture.path().join("victim");
        std::fs::write(&victim, "unchanged").unwrap();
        let cache = agents_status_cache_path_in(&runtime_tmp, 0x2a);
        symlink(&victim, &cache).unwrap();

        write_agents_status_cache_in(&runtime_tmp, 0x2a, "agents").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "unchanged");
        let metadata = std::fs::symlink_metadata(&cache).unwrap();
        assert!(metadata.is_file());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(
            metadata.uid(),
            std::fs::symlink_metadata(&runtime_tmp).unwrap().uid()
        );
        assert_eq!(std::fs::read_to_string(cache).unwrap(), "agents");
    }
}

#[cfg(test)]
mod mcp_cli_tests {
    use super::*;

    #[test]
    fn mcp_rejects_http_token_argument() {
        // Given an HTTP MCP wrapper invocation containing the removed secret flag.
        let args = [
            "rtrt",
            "mcp",
            "--transport",
            "http",
            "--http-token",
            "secret",
        ];

        // When clap parses the invocation.
        let result = Cli::try_parse_from(args);

        // Then the secret-bearing argument is rejected.
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod startup_stack {
    use super::*;

    /// Regression guard for the Windows STATUS_STACK_OVERFLOW (0xC00000FD) that
    /// aborted every `rtrt` invocation — including `rtrt --version` — on
    /// Windows CI. clap's derive-generated `Command` builder for rtrt's ~40
    /// subcommands compiles to a huge (unoptimized) frame that overflows a
    /// 1 MiB stack, and Windows' default main-thread stack is exactly 1 MiB.
    /// `main` now runs parse+dispatch on a `MAIN_STACK_SIZE` worker thread.
    ///
    /// This test builds and parses the `Command` on a thread sized to
    /// `MAIN_STACK_SIZE` and asserts it completes. It runs on every platform:
    /// clap's builder provably overflows a 1 MiB stack (see the crash this
    /// fixed), so lowering `MAIN_STACK_SIZE` below what the builder needs would
    /// make this test overflow here too — catching the regression before it
    /// reaches Windows.
    #[test]
    fn parse_fits_configured_worker_stack() {
        let worker = std::thread::Builder::new()
            .stack_size(MAIN_STACK_SIZE)
            .spawn(|| {
                use clap::{Parser, error::ErrorKind};
                // `try_parse_from` builds the full Command tree (the stack hog)
                // and returns Err(DisplayVersion) instead of calling exit().
                let err = Cli::try_parse_from(["rtrt", "--version"])
                    .expect_err("--version short-circuits parsing");
                assert_eq!(err.kind(), ErrorKind::DisplayVersion);
            })
            .expect("spawn worker thread");
        worker
            .join()
            .expect("clap parse must fit within MAIN_STACK_SIZE");
    }
}
