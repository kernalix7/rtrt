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
    /// BERTScore F1 between every fixture sample and its `Compressor::compress`
    /// output. Requires `--features bertscore`, an ONNX encoder, and the
    /// matching `tokenizer.json`.
    #[cfg(feature = "bertscore")]
    Bertscore {
        /// ONNX encoder (BERT-base / DistilBERT / MiniLM, …) emitting
        /// `[1, seq_len, hidden]` per-token embeddings.
        #[arg(long, env = "RTRT_BERTSCORE_MODEL")]
        model: PathBuf,
        /// HuggingFace tokenizer.json matching the encoder.
        #[arg(long, env = "RTRT_BERTSCORE_TOKENIZER")]
        tokenizer: PathBuf,
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Compression level to evaluate. Defaults to `full`.
        #[arg(long, default_value = "full")]
        level: String,
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
        #[cfg(feature = "bertscore")]
        Cmd::Bertscore {
            model,
            tokenizer,
            fixture,
            level,
            json,
        } => {
            let f = load_compress_fixture(fixture.as_deref())?;
            let scorer = rtrt_eval::bertscore::BertScoreScorer::new(&model, &tokenizer)?;
            let parsed = match level.as_str() {
                "lite" => CompressionLevel::Lite,
                "full" => CompressionLevel::Full,
                "ultra" => CompressionLevel::Ultra,
                other => anyhow::bail!("unknown level: {other}"),
            };
            let report = scorer.evaluate_fixture(&f, parsed)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "# bertscore fixture={} model={}",
                    report.fixture, report.model
                );
                println!(
                    "  {:<22}  {:>7}  {:>7}  {:>6}  {:>6}  {:>6}",
                    "sample", "orig", "new", "P", "R", "F1"
                );
                for s in &report.samples {
                    println!(
                        "  {:<22}  {:>7}  {:>7}  {:>6.3}  {:>6.3}  {:>6.3}",
                        s.id, s.original_chars, s.compressed_chars, s.precision, s.recall, s.f1
                    );
                }
                println!(
                    "  {:<22}  {:>7}  {:>7}  {:>6.3}  {:>6.3}  {:>6.3}",
                    "(mean)", "", "", report.mean_precision, report.mean_recall, report.mean_f1
                );
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
