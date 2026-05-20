//! rtrt — top-level CLI for the Rust Token Reduction Toolkit.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

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
    /// Show RTRT version + crate manifest.
    Info,
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
                println!(
                    "{:<18} [{:?}]  {}",
                    t.name, t.source, t.description
                );
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
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("app")
                    .to_string()
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
        Cmd::Info => {
            println!("rtrt v{}", env!("CARGO_PKG_VERSION"));
            println!("crates: core, compress, proxy, memory, providers, templates, mcp, dashboard, cli");
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
