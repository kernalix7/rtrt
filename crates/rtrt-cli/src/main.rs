//! rtrt — top-level CLI for the Rust Token Reduction Toolkit.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
mod setup;

use rtrt_compress::{
    AsyncCompressor, Compressor, Language as TsLanguage, LlmCompressor, SignatureExtractor,
};
use rtrt_core::CompressionLevel;
use rtrt_memory::{LlmSummariser, MemoryStore};
use rtrt_providers::{
    AnthropicProvider, ChatMessage, ChatRequest, ChatStreamEvent, Context7Client,
    OpenAICompatibleProvider, OpenAIProvider, Provider, Role,
};
use rtrt_templates::PromptRegistry;
use setup::{AgentKind, SetupPlan};

#[derive(Debug, Parser)]
#[command(name = "rtrt", version, about = "Rust-based Token Reduction Toolkit", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Compress text read from stdin.
    Compress {
        #[arg(short, long, value_enum, default_value = "full")]
        level: LevelArg,
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
    },
    /// Filter a command output (read from stdin) for a given command.
    Proxy {
        /// Command being run (e.g. "git status").
        command: String,
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
    /// Talk to a chat provider.
    Provider {
        #[command(subcommand)]
        cmd: ProviderCmd,
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
        #[arg(long, default_value = "127.0.0.1:3112")]
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
    /// Wire RTRT into a popular coding agent's MCP config.
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
        /// Optional file-name suffix filter (default: `.rs`).
        #[arg(long, default_value = ".rs")]
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
    /// Scan shell history for commands routable through `rtrt proxy`.
    Discover {
        /// History file path. Defaults to `~/.zsh_history` then `~/.bash_history`.
        #[arg(long)]
        history: Option<PathBuf>,
        /// Top-N commands to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show RTRT version + crate manifest.
    Info,
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
        #[arg(long, default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
    },
    /// Recall memories by BM25 (FTS5).
    Recall {
        #[arg(long)]
        project: String,
        #[arg(long)]
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
        /// qdrant-style payload filter (e.g. `source=claude,topic~^auth`).
        #[arg(long)]
        filter: Option<String>,
    },
    /// Export every memory row in a project to JSON Lines (stdout if `--out` omitted).
    Export {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = ".rtrt/memory.sqlite")]
        store: PathBuf,
        /// Destination file. `-` (or omit) writes to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import JSON Lines emitted by `rtrt memory export` (stdin if `--in` omitted).
    Import {
        #[arg(long, default_value = ".rtrt/memory.sqlite")]
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
        #[arg(long, default_value = ".rtrt/memory.sqlite")]
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
        #[arg(long, default_value = ".rtrt/memory.sqlite")]
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info")
        .init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Compress {
            level,
            llm,
            provider,
            model,
            base_url,
            format,
            ml,
            ratio,
        } => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            if llm {
                let model = model.ok_or_else(|| anyhow::anyhow!("--llm requires --model"))?;
                let kind = provider.unwrap_or_else(|| detect_provider(&model));
                let provider = build_provider(kind, base_url, &model)?;
                let compressor = LlmCompressor::new(provider, model);
                let out = compressor.compress(&buf).await?;
                print!("{out}");
            } else if ml {
                let target = rtrt_compress::CompressionTarget::new(ratio)?;
                let compressor = rtrt_compress::MlCompressor::heuristic();
                print!("{}", compressor.compress(&buf, target));
            } else {
                let compressor = Compressor::new(level.into());
                let out = compressor.compress_to(&buf, format.into());
                print!("{out}");
            }
        }
        Cmd::Proxy { command } => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let out = match rtrt_proxy::filter_for(&command) {
                Some(f) => f.apply(&buf),
                None => buf,
            };
            print!("{out}");
        }
        Cmd::Templates => {
            for t in rtrt_templates::list_all() {
                println!("{:<18} [{:?}]  {}", t.name, t.source, t.description);
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
        Cmd::Setup {
            agent,
            apply,
            memory,
            binary,
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
            })?;
        }
        Cmd::RepoMap {
            root,
            max_bytes,
            ext,
        } => {
            let lang = TsLanguage::Rust;
            let extractor = SignatureExtractor::new(lang);
            let mut entries: Vec<(PathBuf, String, usize, usize)> = Vec::new();
            for entry in walk_dir(&root) {
                if !entry.is_file() {
                    continue;
                }
                if !entry.to_string_lossy().ends_with(&ext) {
                    continue;
                }
                let size = std::fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
                if size > max_bytes {
                    continue;
                }
                let src = match std::fs::read_to_string(&entry) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
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
        Cmd::Discover { history, limit } => {
            let path = history.or_else(default_history_path).ok_or_else(|| {
                anyhow::anyhow!(
                    "no shell history found; pass --history <path> (zsh: ~/.zsh_history, bash: ~/.bash_history)"
                )
            })?;
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let raw = String::from_utf8_lossy(&bytes);
            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for line in raw.lines() {
                // zsh extended history: ": <ts>:<dur>;<cmd>"
                let cmd = line
                    .strip_prefix(": ")
                    .and_then(|s| s.split_once(';').map(|x| x.1))
                    .unwrap_or(line)
                    .trim();
                if cmd.is_empty() {
                    continue;
                }
                if let Some(f) = rtrt_proxy::filter_for(cmd) {
                    *counts.entry(f.command.to_string()).or_insert(0) += 1;
                }
            }
            let mut sorted: Vec<_> = counts.into_iter().collect();
            sorted.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            sorted.truncate(limit);
            if sorted.is_empty() {
                println!("no proxy-eligible commands found in {}.", path.display());
                println!(
                    "(rtrt-proxy currently ships filters for git status, git log, cargo build, cargo test.)"
                );
            } else {
                println!("== discover: {} ==", path.display());
                let total: usize = sorted.iter().map(|(_, n)| n).sum();
                for (cmd, n) in &sorted {
                    println!("{:>6}× {}", n, cmd);
                }
                println!();
                println!(
                    "total: {total} eligible invocation(s). Pipe through `rtrt proxy \"<cmd>\"`."
                );
            }
        }
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
    }
    Ok(())
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

async fn run_memory(cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::Save {
            project,
            kind,
            body,
            store,
        } => {
            let store = MemoryStore::open(&store)?;
            let body = read_body_or_stdin(body)?;
            let id = store.save(&project, &kind, &body)?;
            println!("saved id={id}");
        }
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
