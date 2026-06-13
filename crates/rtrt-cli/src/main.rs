//! rtrt (Retort) — top-level CLI for the Rust toolkit that distills AI agent context.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
mod proxy_stats;
mod security;
mod service;
mod setup;

use rtrt_compress::{
    AsyncCompressor, Compressor, Language as TsLanguage, LlmCompressor, SignatureExtractor,
};
use rtrt_core::{
    Capability, CompressionLevel, CostClass, DetectedTool, InvocationMode, OutputStyleLevel,
    ToolKind,
};
use rtrt_memory::{LlmSummariser, MemoryStore};
use rtrt_providers::{
    AnthropicProvider, ChatMessage, ChatRequest, ChatStreamEvent, Context7Client,
    DEFAULT_TIMEOUT_SECS, InvokeOptions, Mode as InvokeMode, OpenAICompatibleProvider,
    OpenAIProvider, Prefer, Provider, Role, RouteDecision, RouteRequest, UsageSnapshot,
    invoke_agent, select_route,
};
use rtrt_templates::PromptRegistry;
use setup::{AgentKind, SetupPlan};

#[derive(Debug, Parser)]
#[command(name = "rtrt", version, about = "Retort — a Rust toolkit that distills AI agent context (memory, compression, proxy, routing)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Compress text read from stdin or --file.
    Compress {
        #[arg(short, long, value_enum, default_value = "full")]
        level: LevelArg,
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
    Stats,
    /// Show Command Optimizer savings.
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
    Proxy {
        /// Command being run (e.g. "git status").
        command: String,
    },
    /// Run a shell command through the Command Optimizer.
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
    Templates,
    /// Scaffold a new project from a template.
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
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Talk to a chat provider.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
    },
    /// Invoke a detected local agent or provider through the cross-tool bridge.
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
        /// Prompt text. Multiple words are joined with spaces.
        #[arg(num_args = 1.., allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Pick and optionally invoke the cheapest useful route for a prompt.
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
        /// Prompt text. Multiple words are joined with spaces.
        #[arg(num_args = 1.., allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Persistent memory operations (SQLite-backed).
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Run a Criterion benchmark from the workspace and summarise savings.
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
        /// Path to the SQLite memory store.
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        memory: PathBuf,
        /// Bearer token for HTTP transport. Reads from RTRT_MCP_HTTP_TOKEN by default.
        #[arg(long, env = "RTRT_MCP_HTTP_TOKEN")]
        http_token: Option<String>,
        /// Allowed Origins (comma-separated) for HTTP transport.
        #[arg(long, env = "RTRT_MCP_ALLOWED_ORIGINS", value_delimiter = ',')]
        allowed_origins: Vec<String>,
        /// Override the discovered `rtrt-mcp` binary path.
        #[arg(long)]
        binary: Option<PathBuf>,
    },
    /// Reverse a previous `rtrt setup`. Drops the `rtrt` MCP entry from the
    /// agent's config; with `--plugin`, also removes the rtrt hook entries
    /// from `~/.claude/settings.json`. Dry-run by default;
    /// pass `--apply` to actually delete.
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
    /// Capture a hook payload — used by the `~/.claude/settings.json`
    /// entries that `rtrt setup --plugin --apply` installs. Reads the
    /// payload on stdin, strips control bytes, applies `redact_secrets`,
    /// and writes a memory row with the supplied kind. Exits 0 even on
    /// error so a hook never blocks the host agent.
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Print the Output Optimizer statusline badge or rich Claude Code line.
    Statusline {
        /// Force rich mode even when stdin is a TTY.
        #[arg(long)]
        rich: bool,
        /// Override the first rich statusline template.
        #[arg(long)]
        format: Option<String>,
    },
    /// Wire RTRT into a popular coding agent's MCP config. `--plugin`
    /// (Claude only) also merges hook entries into
    /// `~/.claude/settings.json` so every PreToolUse / PostToolUse /
    /// SessionStart etc. auto-captures into the memory store.
    Setup {
        /// Target agent.
        #[arg(short, long, value_enum)]
        agent: AgentKind,
        /// Apply the change. Without this, only a dry-run snippet is printed.
        #[arg(long)]
        apply: bool,
        /// Path to the memory store (passed to `rtrt-mcp --memory`).
        #[arg(long)]
        memory: Option<PathBuf>,
        /// Override the discovered `rtrt-mcp` binary path.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Also install the Claude Code hook entries and status line
        /// (only valid with `--agent claude`).
        #[arg(long)]
        plugin: bool,
    },
    /// Run `rtrt-dashboard` as a background OS service so it starts on login
    /// and restarts on crash (systemd --user on Linux, launchd on macOS).
    /// Dry-run by default; pass `--apply`.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Security scanning and profile management.
    Security {
        #[command(subcommand)]
        cmd: security::SecurityCmd,
    },
    /// Extract top-level signatures from source via tree-sitter (drops bodies).
    Signatures {
        /// Language. Currently: `rust`.
        #[arg(long, default_value = "rust")]
        lang: String,
    },
    /// Versioned prompt registry (file-backed under ~/.rtrt/prompts/).
    Prompt {
        #[command(subcommand)]
        cmd: PromptCmd,
    },
    /// Walk a directory and emit a tree-sitter signature map of every Rust file.
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
    Context {
        #[command(subcommand)]
        cmd: ContextCmd,
    },
    /// Run a command, capture failures, then ask an LLM for a fix suggestion.
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
    Info,
    /// Manage the global config file (`~/.rtrt/config.toml`).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
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
        project: String,
        #[arg(long, default_value = "note")]
        kind: String,
        body: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
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
        project: String,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
        /// qdrant-style payload filter (e.g. `source=claude,topic~^auth`).
        #[arg(long)]
        filter: Option<String>,
    },
    /// Export every memory row in a project to JSON Lines (stdout if `--out` omitted).
    Export {
        #[arg(long)]
        project: String,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
        /// Destination file. `-` (or omit) writes to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import JSON Lines emitted by `rtrt memory export` (stdin if `--in` omitted).
    Import {
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
        /// Source file. `-` (or omit) reads from stdin.
        #[arg(long = "in")]
        input: Option<PathBuf>,
    },
    /// Extract atomic facts from a passage via LLM and save each.
    Extract {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "note")]
        kind: String,
        body: Option<String>,
        #[arg(short, long, value_enum)]
        provider: ProviderArg,
        #[arg(short, long)]
        model: String,
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
    },
    /// Compress old memories — keep the most recent N, summarise the rest.
    Compress {
        #[arg(long)]
        project: String,
        #[arg(long, default_value_t = 20)]
        keep: usize,
        #[arg(short, long, value_enum)]
        provider: ProviderArg,
        #[arg(short, long)]
        model: String,
        #[arg(long, env = "RTRT_PROVIDER_BASE_URL")]
        base_url: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
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
        project: String,
        name: String,
        body: Option<String>,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
    },
    /// Print one block.
    Get {
        #[arg(long)]
        project: String,
        name: String,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
    },
    /// List every block in a project.
    List {
        #[arg(long)]
        project: String,
        #[arg(long, env = "RTRT_MEMORY_PATH", default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
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
const PROJECT_STATUSLINE_NEEDLE: &str = "statusline --rich";
const PROJECT_HOOK_NEEDLE: &str = "rtrt hook";
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
    if repair.actions.is_empty() {
        println!("skip: CLAUDE.md managed sections and project agents already present");
    } else {
        for action in &repair.actions {
            print_repair_action(action, dry_run);
        }
    }
    if apply {
        backup_repo_files_for_repair(&repair)?;
        rtrt_templates::project::apply_repair(&repair)?;
    }

    println!("\nSTEP 2 — Activate rtrt features to canonical settings");
    setup::run(SetupPlan {
        agent: AgentKind::Claude,
        apply,
        memory_path: None,
        binary: mcp_binary,
        plugin: true,
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
            default_memory_path().display()
        );
    } else {
        let path = default_memory_path();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectCheckState {
    Pass,
    Warn,
    Fail,
}

impl ProjectCheckState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
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

struct ClaudeSettingsStatus {
    hooks_state: ProjectCheckState,
    hooks_detail: String,
    statusline_state: ProjectCheckState,
    statusline_detail: String,
}

fn claude_settings_status(health: bool) -> ClaudeSettingsStatus {
    let missing = ClaudeSettingsStatus {
        hooks_state: ProjectCheckState::Warn,
        hooks_detail: "settings file missing".into(),
        statusline_state: ProjectCheckState::Warn,
        statusline_detail: "settings file missing".into(),
    };
    let Some(settings_path) = home_dir().map(|home| home.join(".claude/settings.json")) else {
        return ClaudeSettingsStatus {
            hooks_detail: "home directory unavailable".into(),
            statusline_detail: "home directory unavailable".into(),
            ..missing
        };
    };
    if !settings_path.exists() {
        return missing;
    }
    let raw = match std::fs::read_to_string(&settings_path) {
        Ok(raw) => raw,
        Err(err) => {
            let state = if health {
                ProjectCheckState::Fail
            } else {
                ProjectCheckState::Warn
            };
            return ClaudeSettingsStatus {
                hooks_state: state,
                hooks_detail: format!("read failed: {err}"),
                statusline_state: state,
                statusline_detail: format!("read failed: {err}"),
            };
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            let state = if health {
                ProjectCheckState::Fail
            } else {
                ProjectCheckState::Warn
            };
            return ClaudeSettingsStatus {
                hooks_state: state,
                hooks_detail: format!("invalid JSON: {err}"),
                statusline_state: state,
                statusline_detail: format!("invalid JSON: {err}"),
            };
        }
    };
    let hooks_present = parsed
        .get("hooks")
        .is_some_and(|hooks| json_contains_text(hooks, PROJECT_HOOK_NEEDLE));
    let statusline_present = parsed
        .get("statusLine")
        .is_some_and(|statusline| json_contains_text(statusline, PROJECT_STATUSLINE_NEEDLE));
    ClaudeSettingsStatus {
        hooks_state: if hooks_present {
            ProjectCheckState::Pass
        } else {
            ProjectCheckState::Warn
        },
        hooks_detail: if hooks_present {
            "rtrt hook entries found".into()
        } else {
            "rtrt hook entries missing".into()
        },
        statusline_state: if statusline_present {
            ProjectCheckState::Pass
        } else {
            ProjectCheckState::Warn
        },
        statusline_detail: if statusline_present {
            "rtrt rich statusLine found".into()
        } else {
            "rtrt rich statusLine missing".into()
        },
    }
}

fn json_contains_text(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(s) => s.contains(needle),
        serde_json::Value::Array(items) => {
            items.iter().any(|item| json_contains_text(item, needle))
        }
        serde_json::Value::Object(map) => map.values().any(|item| json_contains_text(item, needle)),
        _ => false,
    }
}

fn memory_reachable_status(health: bool) -> (ProjectCheckState, String) {
    let path = std::env::var_os("RTRT_MEMORY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(default_memory_path);
    if !path.exists() {
        return (
            ProjectCheckState::Warn,
            format!("missing {}", path.display()),
        );
    }
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match rusqlite::Connection::open_with_flags(&path, flags)
        .and_then(|conn| conn.query_row("SELECT 1", [], |_row| Ok(())))
    {
        Ok(()) => (
            ProjectCheckState::Pass,
            format!("reachable {}", path.display()),
        ),
        Err(err) => (
            if health {
                ProjectCheckState::Fail
            } else {
                ProjectCheckState::Warn
            },
            format!("unreachable {}: {err}", path.display()),
        ),
    }
}

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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info")
        .init();
    let cli = Cli::parse();
    match cli.command {
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
            prompt,
        } => {
            let prompt = prompt.join(" ");
            if prompt.trim().is_empty() {
                bail!("rtrt call: prompt is empty");
            }
            let outcome = invoke_agent(
                &target,
                &prompt,
                InvokeOptions {
                    mode: Some(mode.into()),
                    model,
                    timeout: std::time::Duration::from_secs(timeout),
                },
            )
            .await
            .with_context(|| format!("rtrt call {target}"))?;
            match format {
                CallFormatArg::Text => print!("{}", outcome.output),
                CallFormatArg::Json => println!("{}", serde_json::to_string_pretty(&outcome)?),
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
                prompt,
            })
            .await?;
        }
        Cmd::Provider { cmd } => run_provider(cmd).await?,
        Cmd::Memory { cmd } => run_memory(cmd).await?,
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
            memory,
            http_token,
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
            cmd.arg("--memory").arg(&memory);
            cmd.arg("--transport").arg(&transport);
            if transport == "http" {
                cmd.arg("--bind").arg(&bind);
                cmd.arg("--path").arg(&path);
                if let Some(tok) = http_token.as_deref() {
                    cmd.env("RTRT_MCP_HTTP_TOKEN", tok);
                }
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
        Cmd::Statusline { rich, format } => {
            print_statusline(StatuslineOptions { rich, format });
        }
        Cmd::Setup {
            agent,
            apply,
            memory,
            binary,
            plugin,
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
                memory_path: memory,
                binary,
                plugin,
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
    level: LevelArg,
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
        let compressor = Compressor::new(opts.level.into());
        compressor.compress_to(&input, opts.format.into())
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
    let path = std::env::var_os("RTRT_MEMORY_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(default_memory_path);
    if !path.exists() {
        println!("memory: unavailable ({} not found)", path.display());
        println!("savings: unavailable (memory store unavailable)");
        for source in SAVINGS_SOURCES {
            println!("  {source}: unavailable");
        }
        return;
    }
    let store = match MemoryStore::open(&path) {
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
    let projects = match store.projects() {
        Ok(projects) => projects,
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
    let mut tools = rtrt_core::detect_tools();
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
    prompt: Vec<String>,
}

async fn run_route(opts: RouteCliOptions) -> Result<()> {
    let prompt = opts.prompt.join(" ");
    if prompt.trim().is_empty() {
        bail!("rtrt route: prompt is empty");
    }
    let mode = InvokeMode::from(opts.mode);
    let req = RouteRequest {
        capability: opts.capability.map(Capability::from),
        prefer: Prefer::from(opts.prefer),
        target: opts.target,
        model: opts.model,
        mode: (mode != InvokeMode::Auto).then_some(mode),
    };
    let tools = rtrt_core::detect_tools();
    let usage = UsageSnapshot::load_best_effort();
    let decision = select_route(&req, &tools, &usage)?;

    if opts.explain || opts.dry_run {
        print_route_decision(&decision, opts.explain, &usage);
    }
    if opts.dry_run {
        return Ok(());
    }

    let outcome = invoke_agent(
        &decision.target,
        &prompt,
        InvokeOptions {
            mode: Some(decision.mode),
            model: decision.model.clone(),
            timeout: std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        },
    )
    .await
    .with_context(|| format!("rtrt route {}", decision.target))?;
    print!("{}", outcome.output);
    Ok(())
}

fn print_route_decision(decision: &RouteDecision, explain: bool, usage: &UsageSnapshot) {
    println!("target: {}", decision.target);
    println!("mode: {}", invoke_mode_label(decision.mode));
    println!("model: {}", decision.model.as_deref().unwrap_or("-"));
    println!("cost: {}", cost_class_label(decision.cost_class));
    println!("reason: {}", decision.reason);
    if !explain {
        return;
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
    } else {
        let resp = provider.chat(req).await?;
        println!("{}", resp.content);
        eprintln!(
            "[usage] provider={} model={} input={} output={}",
            resp.provider, resp.model, resp.usage.input_tokens, resp.usage.output_tokens
        );
    }
    Ok(())
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

fn default_memory_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rtrt")
        .join("memory.sqlite")
}

/// Resolve the project bucket for a hook command. Explicit `--project`
/// wins first, then `$RTRT_PROJECT`; otherwise the bucket is derived from
/// the **git repository root** of the current working directory (so
/// subagents / subdirectories / git worktrees fold into the real project)
/// via [`rtrt_core::project_for_cwd`], falling back to the cwd basename and
/// finally `"default"` when there is no cwd.
fn resolve_hook_project(explicit: Option<String>) -> String {
    explicit
        .or_else(|| std::env::var("RTRT_PROJECT").ok())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| rtrt_core::project_for_cwd(&p))
        })
        .unwrap_or_else(|| "default".to_string())
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
    let project = resolve_hook_project(project);
    let store_path = store.unwrap_or_else(default_memory_path);
    if !store_path.exists() {
        return Ok(());
    }
    let age_sec: i64 = env_or("RTRT_AUTO_COMPRESS_AGE_SEC", cfg.age_sec);
    let min_chars: usize = env_or("RTRT_AUTO_COMPRESS_MIN_CHARS", cfg.min_chars);
    let batch: usize = env_or("RTRT_AUTO_COMPRESS_BATCH", cfg.batch);
    let model = std::env::var("RTRT_AUTO_COMPRESS_MODEL").unwrap_or_else(|_| cfg.model.clone());
    let max_tokens: u32 = env_or("RTRT_AUTO_COMPRESS_MAX_TOKENS", cfg.max_tokens);
    let memory = MemoryStore::open(&store_path)?;
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
            let project = resolve_hook_project(project);
            let store_path = store.unwrap_or_else(default_memory_path);
            if let Some(parent) = store_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let memory = MemoryStore::open(&store_path)
                .with_context(|| format!("open memory store {}", store_path.display()))?;
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
    five_hour_resets_at: Option<String>,
    seven_day_pct: Option<u64>,
    seven_day_resets_at: Option<String>,
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
    let mut cfg = load_statusline_config();
    if let Some(format) = format_override {
        cfg.format = format;
    }
    let input = parse_claude_status_input(raw_stdin);
    let cwd = input
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
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
    let opt_level = rtrt_core::read_output_style_level_for(Some(&repo_root_from_cwd(&cwd)));
    segments.insert("opt".to_string(), format!("opt:{}", opt_level.as_str()));
    if let Some(usage) = usage_seg {
        segments.insert("usage".to_string(), usage);
    }
    let agents = agents_segment(cfg.codex_check_timeout_ms, statusline_agents_width_budget());
    segments.insert("agents".to_string(), agents.clone());
    segments.insert("codex".to_string(), agents);
    segments.insert(
        "savings".to_string(),
        format!("💯Σ:{}", total_savings_tokens()),
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

fn load_statusline_config() -> StatuslineConfig {
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
        five_hour_resets_at: json_string(five_hour, "resets_at"),
        seven_day_pct: json_round_pct(seven_day, "used_percentage"),
        seven_day_resets_at: json_string(seven_day, "resets_at"),
    }
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64);
    let window = |pct: Option<u64>, resets_at: Option<&str>| -> Option<String> {
        let pct = pct?;
        let reset = match (now, resets_at.and_then(rfc3339_to_epoch)) {
            (Some(now), Some(reset)) => format!(" ↻{}", humanize_remaining(reset - now)),
            _ => String::new(),
        };
        Some(format!("{pct}%{reset}"))
    };
    let five = window(input.five_hour_pct, input.five_hour_resets_at.as_deref());
    let seven = window(input.seven_day_pct, input.seven_day_resets_at.as_deref());
    if five.is_none() && seven.is_none() {
        return None;
    }
    let five = five.unwrap_or_else(|| "—".to_string());
    let seven = seven.unwrap_or_else(|| "—".to_string());
    Some(format!("5h:{five} | wk:{seven}"))
}

fn agents_segment(timeout_ms: u64, width_budget: usize) -> String {
    if let Some(cached) = read_agents_status_cache() {
        return cached;
    }
    let timeout = std::time::Duration::from_millis(timeout_ms.max(1));
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("rtrt-statusline-agents".into())
        .spawn(move || {
            let names = detected_statusline_agents();
            let _ = tx.send(names);
        });
    let names = rx
        .recv_timeout(timeout)
        .unwrap_or_else(|_| cheap_statusline_agents());
    let segment = format_agents_segment(&names, width_budget);
    write_agents_status_cache(&segment);
    segment
}

fn detected_statusline_agents() -> Vec<String> {
    let mut names = Vec::new();
    for tool in rtrt_core::detect_tools().into_iter().filter(|tool| {
        matches!(tool.kind, ToolKind::CodingAgent | ToolKind::LocalRuntime)
            && tool.installed
            && tool.enabled
    }) {
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

fn cheap_statusline_agents() -> Vec<String> {
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("claude", &["claude"]),
        ("codex", &["codex"]),
        ("opencode", &["opencode"]),
        ("ollama", &["ollama"]),
        ("aider", &["aider"]),
        ("cursor", &["cursor-agent", "cursor"]),
        ("gemini", &["gemini"]),
        ("gh", &["gh"]),
    ];
    CANDIDATES
        .iter()
        .filter(|(_name, bins)| bins.iter().any(|bin| binary_on_path(bin)))
        .map(|(name, _bins)| (*name).to_string())
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

fn read_agents_status_cache() -> Option<String> {
    let path = agents_status_cache_path();
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

fn write_agents_status_cache(status: &str) {
    let _ = std::fs::write(agents_status_cache_path(), status);
}

fn agents_status_cache_path() -> PathBuf {
    std::env::temp_dir().join("rtrt-agents-status.cache")
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Best-effort HYBRID recall for the auto-recall hook.
///
/// Returns `Some(hits)` only when a hybrid (BM25 + dense-vector RRF) recall
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
fn try_hybrid_recall(
    store_path: &std::path::Path,
    project: &str,
    query: &str,
    limit: usize,
    timeout: std::time::Duration,
) -> Option<Vec<rtrt_memory::MemoryRecord>> {
    let cfg = rtrt_core::Config::load().unwrap_or_default();
    let ecfg = cfg.embeddings;
    // Gate 1: embeddings must be enabled (honours RTRT_EMBED_ENABLED).
    if !ecfg.is_enabled() {
        return None;
    }
    // Gate 2: meaningful embedding coverage for this project. Probing coverage
    // is a cheap local SQL read (no network), so do it before touching Ollama.
    let coverage_store = MemoryStore::open(store_path).ok()?;
    let (embedded, total) = coverage_store.embedding_coverage(project).ok()?;
    if embedded == 0 || embedded.saturating_mul(2) < total {
        return None;
    }
    drop(coverage_store);

    // Gate 3: build the embedder (mirrors the dashboard daemon's resolution:
    // embeddings.base_url → auto_compress.base_url → Ollama default).
    let base_url = ecfg.resolved_base_url(cfg.auto_compress.base_url.as_deref());
    let model = ecfg.effective_model();

    // Run the hybrid recall on a worker thread bounded by `timeout`. The thread
    // opens its OWN MemoryStore on the WAL db (concurrent reads are fine) and
    // its own embedder, so nothing non-Send crosses the boundary. We wait on a
    // bounded channel: if Ollama is slow/unreachable we stop waiting after
    // `timeout` and the caller falls back to BM25. The worker is detached and
    // simply finishes (or errors) on its own; its result is discarded.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<Vec<rtrt_memory::MemoryRecord>>>(1);
    let store_path = store_path.to_path_buf();
    let project = project.to_string();
    let query = query.to_string();
    std::thread::spawn(move || {
        let result = (|| {
            let store = MemoryStore::open(&store_path).ok()?;
            let embedder = rtrt_memory::OllamaEmbedder::new(base_url, model);
            let scored = store
                .recall_hybrid(&project, &query, limit, &embedder)
                .ok()?;
            Some(scored.into_iter().map(|s| s.record).collect::<Vec<_>>())
        })();
        // Ignore send errors: the receiver may have already timed out and gone.
        let _ = tx.send(result);
    });

    // `recv_timeout` returns Err on both timeout and a dropped sender; either
    // way we fall back. A successful hybrid yields `Some(hits)`; an inner error
    // yields `Some(None)` which we also treat as a fall-back signal.
    match rx.recv_timeout(timeout) {
        Ok(Some(hits)) => Some(hits),
        _ => None,
    }
}

fn run_hook_recall(project: Option<String>, store: Option<PathBuf>, limit: usize) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok();
    // The prompt text is either the `prompt` field of a JSON payload or the
    // whole stdin when it isn't JSON.
    let prompt = extract_json_str(&raw, "prompt").unwrap_or_else(|| raw.trim().to_string());
    if prompt.trim().is_empty() {
        return Ok(());
    }
    let project = resolve_hook_project(project);
    let store_path = store.unwrap_or_else(default_memory_path);
    if !store_path.exists() {
        return Ok(());
    }
    let memory = MemoryStore::open(&store_path)?;
    // Build an FTS5 OR query: a natural-language prompt joined with spaces
    // is treated as implicit AND by FTS5, which almost never matches a
    // terse memory row. OR-joining the content words ranks any row sharing
    // a term, which is what we want for context injection. Stopwords and
    // sub-3-char tokens are dropped to cut noise.
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "this", "that", "how", "does", "what", "why", "you", "are",
        "was", "were", "can", "should", "would", "could", "from", "into", "have", "has",
    ];
    let terms: Vec<String> = prompt
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3 && !STOP.contains(&w.as_str()))
        .take(32)
        .collect();
    if terms.is_empty() {
        return Ok(());
    }
    let query = terms.join(" OR ");

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
    // falls back to BM25 fast. On ANY error/timeout/absent-embedder we use the
    // exact previous behaviour: store.recall_bm25(project, query, limit).
    const HYBRID_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
    let hits = try_hybrid_recall(&store_path, &project, &query, limit, HYBRID_TIMEOUT)
        .unwrap_or_else(|| {
            memory
                .recall_bm25(&project, &query, limit)
                .unwrap_or_default()
        });
    if hits.is_empty() {
        return Ok(());
    }
    // stdout of a UserPromptSubmit hook is injected into the model context.
    println!("## Relevant project memory ({project})");
    for h in hits {
        let body = h.body.replace('\n', " ");
        let clipped: String = body.chars().take(240).collect();
        println!("- [{}] {}", h.kind, clipped);
    }
    Ok(())
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
    let project = resolve_hook_project(project);
    let store_path = store.unwrap_or_else(default_memory_path);
    if !store_path.exists() {
        return Ok(());
    }
    let memory = MemoryStore::open(&store_path)?;
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
        "user-prompt-submit" | "user-prompt-expansion" => get("prompt").trim().to_string(),
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

async fn run_memory(cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::Save {
            project,
            kind,
            body,
            store,
            meta,
        } => {
            let store = MemoryStore::open(&store)?;
            let body = read_body_or_stdin(body)?;
            let id = if meta.is_empty() {
                store.save(&project, &kind, &body)?
            } else {
                let map: std::collections::BTreeMap<String, String> = meta.into_iter().collect();
                store.save_with_metadata(&project, &kind, &body, &map)?
            };
            println!("saved id={id}");
        }
        MemoryCmd::Blocks { cmd } => match cmd {
            BlockCmd::Set {
                project,
                name,
                body,
                store,
            } => {
                let store = MemoryStore::open(&store)?;
                let body = read_body_or_stdin(body)?;
                let id = store.set_block(&project, &name, &body)?;
                println!("block id={id}");
            }
            BlockCmd::Get {
                project,
                name,
                store,
            } => {
                let store = MemoryStore::open(&store)?;
                match store.get_block(&project, &name)? {
                    Some(b) => println!("{}", b.body),
                    None => anyhow::bail!("block not found: {name}"),
                }
            }
            BlockCmd::List { project, store } => {
                let store = MemoryStore::open(&store)?;
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
            let store = MemoryStore::open(&store)?;
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
            let store = MemoryStore::open(&store)?;
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
            let store = MemoryStore::open(&store)?;
            let count = match input {
                Some(p) if p.as_os_str() != "-" => {
                    let f = std::fs::File::open(&p)?;
                    store.import_jsonl(std::io::BufReader::new(f))?
                }
                _ => {
                    let stdin = std::io::stdin();
                    store.import_jsonl(stdin.lock())?
                }
            };
            eprintln!("[rtrt memory import] {count} records");
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
            let store = MemoryStore::open(&store)?;
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
            let store = MemoryStore::open(&store)?;
            let p = build_provider(provider, base_url, &model)?;
            let summariser = LlmSummariser::new(p, model);
            match store.compress_project(&project, &summariser, keep).await? {
                Some(id) => println!("archival id={id}; older entries deleted"),
                None => println!("nothing to compress (have ≤ {keep} entries)"),
            }
        }
    }
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
mod statusline_tests {
    use super::*;

    fn segment_map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
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
}
