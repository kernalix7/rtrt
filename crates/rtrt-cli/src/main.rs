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
    AnthropicProvider, ChatMessage, ChatRequest, ChatStreamEvent, OpenAICompatibleProvider,
    OpenAIProvider, Provider, Role,
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
            } else {
                let out = Compressor::new(level.into()).compress(&buf);
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
            entries.sort_by(|a, b| b.3.cmp(&a.3));
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
            let pct = if total_before == 0 {
                0
            } else {
                (total_before - total_after) * 100 / total_before
            };
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
            sorted.sort_by(|a, b| b.1.cmp(&a.1));
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
        } => {
            let store = MemoryStore::open(&store)?;
            let hits = store.recall_bm25(&project, &query, limit)?;
            for h in hits {
                println!("[{}] {} {}", h.id, h.kind, h.body);
            }
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
