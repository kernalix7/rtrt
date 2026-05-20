//! rtrt — top-level CLI for the Rust Token Reduction Toolkit.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;
use rtrt_memory::{LlmSummariser, MemoryStore};
use rtrt_providers::{
    AnthropicProvider, ChatMessage, ChatRequest, ChatStreamEvent, OpenAICompatibleProvider,
    OpenAIProvider, Provider, Role,
};

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
}

impl From<LevelArg> for CompressionLevel {
    fn from(l: LevelArg) -> Self {
        match l {
            LevelArg::Lite => CompressionLevel::Lite,
            LevelArg::Full => CompressionLevel::Full,
            LevelArg::Ultra => CompressionLevel::Ultra,
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
    let (k, v) = s.split_once('=').ok_or_else(|| format!("expected key=value, got `{s}`"))?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("rtrt=info").init();
    let cli = Cli::parse();
    match cli.command {
        Cmd::Compress { level } => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            let out = Compressor::new(level.into()).compress(&buf);
            print!("{out}");
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
        Cmd::New { template, path, vars, overwrite, no_hooks } => {
            let tmpl = rtrt_templates::find(&template)
                .with_context(|| format!("unknown template: {template}"))?;
            let mut map = BTreeMap::new();
            for (k, v) in vars {
                map.insert(k, v);
            }
            map.entry("project_name".into()).or_insert_with(|| {
                path.file_name().and_then(|s| s.to_str()).unwrap_or("app").to_string()
            });
            let plan = rtrt_templates::render::plan(&tmpl, &path, map)?;
            rtrt_templates::render::write(&plan, overwrite)?;
            println!("scaffolded {} files into {}", plan.files.len(), plan.root.display());
            if !no_hooks {
                for hook in &plan.post_hooks {
                    println!("$ {hook}");
                    run_hook(&plan.root, hook)?;
                }
            }
        }
        Cmd::Provider { cmd } => run_provider(cmd).await?,
        Cmd::Memory { cmd } => run_memory(cmd).await?,
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
    let status = std::process::Command::new(bin).args(args).current_dir(cwd).status()?;
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
        messages.push(ChatMessage { role: Role::System, content: sys });
    }
    messages.push(ChatMessage { role: Role::User, content: text });
    let req =
        ChatRequest { model: model.clone(), messages, max_tokens, temperature: None };

    let provider: Box<dyn Provider> = match kind {
        ProviderArg::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .context("ANTHROPIC_API_KEY not set")?;
            Box::new(AnthropicProvider::new(key))
        }
        ProviderArg::Openai => {
            let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
            Box::new(OpenAIProvider::new(key))
        }
        ProviderArg::OpenaiCompat => {
            let url = base_url
                .ok_or_else(|| anyhow::anyhow!("--base-url required for openai-compat"))?;
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

async fn run_memory(cmd: MemoryCmd) -> Result<()> {
    match cmd {
        MemoryCmd::Save { project, kind, body, store } => {
            let store = MemoryStore::open(&store)?;
            let body = read_body_or_stdin(body)?;
            let id = store.save(&project, &kind, &body)?;
            println!("saved id={id}");
        }
        MemoryCmd::Recall { project, query, limit, store } => {
            let store = MemoryStore::open(&store)?;
            let hits = store.recall_bm25(&project, &query, limit)?;
            for h in hits {
                println!("[{}] {} {}", h.id, h.kind, h.body);
            }
        }
        MemoryCmd::Extract { project, kind, body, provider, model, base_url, store } => {
            let store = MemoryStore::open(&store)?;
            let body = read_body_or_stdin(body)?;
            let p = build_provider(provider, base_url, &model)?;
            let summariser = LlmSummariser::new(p, model);
            let ids = store.extract_and_save(&project, &kind, &body, &summariser).await?;
            println!("extracted {} fact(s):", ids.len());
            for id in ids {
                println!("  id={id}");
            }
        }
        MemoryCmd::Compress { project, keep, provider, model, base_url, store } => {
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
            let url = base_url
                .ok_or_else(|| anyhow::anyhow!("--base-url required for openai-compat"))?;
            let mut p = OpenAICompatibleProvider::new("openai-compat", url);
            if let Ok(key) = std::env::var("RTRT_PROVIDER_API_KEY") {
                p = p.with_api_key(key);
            }
            Box::new(p)
        }
    };
    Ok(provider)
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
