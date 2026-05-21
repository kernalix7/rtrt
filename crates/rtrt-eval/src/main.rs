//! `rtrt-eval` — opt-in evaluation runner.
//!
//! Two subcommands, both reading either a JSON fixture path or the built-in
//! smoke fixture. Output is either a human table or `--json` for machine
//! consumption (CI gates, dashboards).

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rtrt_core::CompressionLevel;
use rtrt_eval::{
    evaluate_compression, evaluate_recall, load_compress_fixture, load_recall_fixture,
};

#[derive(Parser)]
#[command(
    name = "rtrt-eval",
    version,
    about = "Evaluation harness for rtrt-memory + rtrt-compress"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Evaluate `MemoryStore::recall_bm25` against a labelled fixture.
    Recall {
        /// Fixture JSON path. Defaults to the built-in smoke fixture.
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// K for recall@K and MRR.
        #[arg(long, default_value_t = 5)]
        k: usize,
        /// Emit JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Evaluate `Compressor::compress` at every level against a fixture.
    Compress {
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Restrict to a single level. Default runs all three.
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Recall { fixture, k, json } => {
            let f = load_recall_fixture(fixture.as_deref())?;
            let report = evaluate_recall(&f, k)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("# recall fixture={} k={}", report.fixture, report.k);
                println!("  queries:      {}", report.queries);
                println!("  recall@{}:    {:.3}", report.k, report.recall_at_k);
                println!("  MRR:          {:.3}", report.mrr);
                println!();
                for q in &report.per_query {
                    let status = if q.recall >= 1.0 {
                        "ok"
                    } else if q.recall > 0.0 {
                        "partial"
                    } else {
                        "miss"
                    };
                    println!(
                        "  [{:7}] r={:.2} rr={:.2} q={:?} → {:?}",
                        status, q.recall, q.reciprocal_rank, q.query, q.hits_top_k
                    );
                }
            }
        }
        Cmd::Compress {
            fixture,
            level,
            json,
        } => {
            let f = load_compress_fixture(fixture.as_deref())?;
            let levels: Vec<CompressionLevel> = match level.as_deref() {
                None => vec![
                    CompressionLevel::Lite,
                    CompressionLevel::Full,
                    CompressionLevel::Ultra,
                ],
                Some("lite") => vec![CompressionLevel::Lite],
                Some("full") => vec![CompressionLevel::Full],
                Some("ultra") => vec![CompressionLevel::Ultra],
                Some(other) => anyhow::bail!("unknown level: {other}"),
            };
            let reports: Vec<_> = levels
                .into_iter()
                .map(|lvl| evaluate_compression(&f, lvl))
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                println!("# compress fixture={}", f.name);
                println!(
                    "  {:<8}  {:<22}  {:>7}  {:>7}  {:>6}",
                    "level", "sample", "orig", "new", "ratio"
                );
                for r in &reports {
                    for s in &r.samples {
                        println!(
                            "  {:<8}  {:<22}  {:>7}  {:>7}  {:>6.3}",
                            r.level, s.id, s.original_chars, s.compressed_chars, s.ratio
                        );
                    }
                    println!(
                        "  {:<8}  {:<22}  {:>7}  {:>7}  {:>6.3}",
                        r.level, "(mean)", "", "", r.mean_ratio
                    );
                }
            }
        }
    }
    Ok(())
}
