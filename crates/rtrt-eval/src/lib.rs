//! Evaluation harness for the rtrt toolkit.
//!
//! Two surfaces are covered:
//!
//! - **Recall quality.** `recall_at_k` and `mrr` reduce a `RecallFixture`
//!   (corpus + labelled queries) over `MemoryStore::recall_bm25` and emit a
//!   single `RecallReport` per K. The defaults stay BM25-only so the harness
//!   has no network or model dependencies.
//! - **Compression ratio.** `compression_ratio` measures the char savings of
//!   `Compressor::compress` at every `CompressionLevel` on a `CompressFixture`.
//!
//! Both fixtures are plain JSON so a user can replace the built-in smoke
//! corpus with LongMemEval-S or any in-house dataset without touching code.

use std::collections::BTreeMap;
use std::path::Path;

use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;
use rtrt_memory::MemoryStore;
use serde::{Deserialize, Serialize};

/// Smoke fixture shipped under `crates/rtrt-eval/fixtures/recall_smoke.json`.
pub const RECALL_SMOKE: &str = include_str!("../fixtures/recall_smoke.json");

/// Smoke fixture shipped under `crates/rtrt-eval/fixtures/compress_smoke.json`.
pub const COMPRESS_SMOKE: &str = include_str!("../fixtures/compress_smoke.json");

#[derive(Debug, Deserialize, Serialize)]
pub struct RecallFixture {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub corpus: Vec<CorpusItem>,
    pub queries: Vec<LabelledQuery>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CorpusItem {
    pub id: String,
    pub body: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LabelledQuery {
    pub query: String,
    pub relevant: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CompressFixture {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub samples: Vec<CompressSample>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CompressSample {
    pub id: String,
    pub body: String,
}

#[derive(Debug, Serialize)]
pub struct RecallReport {
    pub fixture: String,
    pub k: usize,
    pub queries: usize,
    pub recall_at_k: f64,
    pub mrr: f64,
    pub per_query: Vec<PerQueryRecall>,
}

#[derive(Debug, Serialize)]
pub struct PerQueryRecall {
    pub query: String,
    pub relevant: Vec<String>,
    pub hits_top_k: Vec<String>,
    pub recall: f64,
    pub reciprocal_rank: f64,
}

#[derive(Debug, Serialize)]
pub struct CompressReport {
    pub fixture: String,
    pub level: String,
    pub samples: Vec<CompressSampleReport>,
    pub mean_ratio: f64,
}

#[derive(Debug, Serialize)]
pub struct CompressSampleReport {
    pub id: String,
    pub original_chars: usize,
    pub compressed_chars: usize,
    pub ratio: f64,
    pub compressed: String,
}

/// Loads a recall fixture from disk; returns `RECALL_SMOKE` when `path` is
/// `None`.
pub fn load_recall_fixture(path: Option<&Path>) -> anyhow::Result<RecallFixture> {
    let raw = match path {
        Some(p) => std::fs::read_to_string(p)?,
        None => RECALL_SMOKE.to_string(),
    };
    Ok(serde_json::from_str(&raw)?)
}

/// Same shape as `load_recall_fixture` but for compression samples.
pub fn load_compress_fixture(path: Option<&Path>) -> anyhow::Result<CompressFixture> {
    let raw = match path {
        Some(p) => std::fs::read_to_string(p)?,
        None => COMPRESS_SMOKE.to_string(),
    };
    Ok(serde_json::from_str(&raw)?)
}

/// Evaluates a `RecallFixture` against `MemoryStore::recall_bm25` and
/// returns `recall@K` plus `MRR` averaged across every labelled query.
///
/// The corpus is loaded into an in-memory SQLite store, one row per
/// `CorpusItem`, under the project name `"eval"`. We keep a side-map from
/// row id → corpus id so we can match against the `relevant` list.
pub fn evaluate_recall(fixture: &RecallFixture, k: usize) -> anyhow::Result<RecallReport> {
    let store =
        MemoryStore::open_in_memory().map_err(|e| anyhow::anyhow!("open in-memory store: {e}"))?;
    let mut id_to_corpus: BTreeMap<i64, String> = BTreeMap::new();
    for item in &fixture.corpus {
        let id = store
            .save("eval", "doc", &item.body)
            .map_err(|e| anyhow::anyhow!("save fixture row: {e}"))?;
        id_to_corpus.insert(id, item.id.clone());
    }

    let mut total_recall = 0.0;
    let mut total_rr = 0.0;
    let mut per_query = Vec::with_capacity(fixture.queries.len());
    for q in &fixture.queries {
        let hits = store
            .recall_bm25("eval", &q.query, k)
            .map_err(|e| anyhow::anyhow!("recall: {e}"))?;
        let hit_ids: Vec<String> = hits
            .iter()
            .filter_map(|h| id_to_corpus.get(&h.id).cloned())
            .collect();
        let relevant_set: std::collections::BTreeSet<&str> =
            q.relevant.iter().map(|s| s.as_str()).collect();
        let found = hit_ids
            .iter()
            .filter(|id| relevant_set.contains(id.as_str()))
            .count();
        let recall = if relevant_set.is_empty() {
            0.0
        } else {
            found as f64 / relevant_set.len() as f64
        };
        let reciprocal_rank = hit_ids
            .iter()
            .enumerate()
            .find(|(_, id)| relevant_set.contains(id.as_str()))
            .map(|(i, _)| 1.0 / (i as f64 + 1.0))
            .unwrap_or(0.0);

        total_recall += recall;
        total_rr += reciprocal_rank;
        per_query.push(PerQueryRecall {
            query: q.query.clone(),
            relevant: q.relevant.clone(),
            hits_top_k: hit_ids,
            recall,
            reciprocal_rank,
        });
    }

    let n = fixture.queries.len() as f64;
    Ok(RecallReport {
        fixture: fixture.name.clone(),
        k,
        queries: fixture.queries.len(),
        recall_at_k: if n == 0.0 { 0.0 } else { total_recall / n },
        mrr: if n == 0.0 { 0.0 } else { total_rr / n },
        per_query,
    })
}

/// Runs every sample through `Compressor::compress` at `level` and reports
/// per-sample + mean compression ratio. Ratio = compressed / original
/// (lower is better).
pub fn evaluate_compression(fixture: &CompressFixture, level: CompressionLevel) -> CompressReport {
    let compressor = Compressor::new(level);
    let mut samples = Vec::with_capacity(fixture.samples.len());
    let mut ratio_sum = 0.0;
    for s in &fixture.samples {
        let out = compressor.compress(&s.body);
        let orig = s.body.chars().count();
        let new = out.chars().count();
        let ratio = if orig == 0 {
            0.0
        } else {
            new as f64 / orig as f64
        };
        ratio_sum += ratio;
        samples.push(CompressSampleReport {
            id: s.id.clone(),
            original_chars: orig,
            compressed_chars: new,
            ratio,
            compressed: out,
        });
    }
    let n = fixture.samples.len() as f64;
    CompressReport {
        fixture: fixture.name.clone(),
        level: format!("{level:?}").to_lowercase(),
        samples,
        mean_ratio: if n == 0.0 { 0.0 } else { ratio_sum / n },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_recall_fixture_parses() {
        let f = load_recall_fixture(None).unwrap();
        assert!(!f.corpus.is_empty());
        assert!(!f.queries.is_empty());
    }

    #[test]
    fn smoke_compress_fixture_parses() {
        let f = load_compress_fixture(None).unwrap();
        assert!(!f.samples.is_empty());
    }

    #[test]
    fn recall_at_5_on_smoke_fixture_clears_floor() {
        let f = load_recall_fixture(None).unwrap();
        let report = evaluate_recall(&f, 5).unwrap();
        // The smoke fixture is hand-tuned so BM25 should land every query
        // above the published target floor (R@5 > 0.80).
        assert!(
            report.recall_at_k >= 0.80,
            "smoke recall@5 = {:.3} below floor",
            report.recall_at_k
        );
        assert!(report.mrr > 0.0);
    }

    #[test]
    fn compress_ultra_shrinks_hedging_sample() {
        let f = load_compress_fixture(None).unwrap();
        let report = evaluate_compression(&f, CompressionLevel::Ultra);
        let hedging = report
            .samples
            .iter()
            .find(|s| s.id == "hedging-heavy")
            .unwrap();
        assert!(
            hedging.ratio < 1.0,
            "ultra compress should shrink hedging-heavy, got ratio={:.3}",
            hedging.ratio
        );
    }
}
