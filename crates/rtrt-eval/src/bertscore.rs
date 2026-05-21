//! BERTScore evaluator — opt-in `bertscore` feature.
//!
//! ## What this measures
//!
//! BERTScore (Zhang et al., 2020) compares two pieces of text by embedding
//! every subword token through a BERT-like encoder and then doing a greedy
//! cosine alignment between the two sequences. Precision is the mean of
//! "for every token in the hypothesis, max cosine against the reference"
//! and recall flips that around. F1 is the harmonic mean.
//!
//! ## Model contract
//!
//! The ONNX encoder must take the standard two inputs and emit a
//! per-subword embedding tensor:
//!
//! | Tensor             | Shape              | dtype |
//! |--------------------|--------------------|-------|
//! | `input_ids`        | `[1, seq_len]`     | i64   |
//! | `attention_mask`   | `[1, seq_len]`     | i64   |
//! | first output       | `[1, seq_len, H]`  | f32   |
//!
//! `H` is the encoder hidden size; we don't care which (BERT-base = 768,
//! distilbert = 768, MiniLM = 384). Special-token positions
//! (`[CLS]` / `[SEP]` — offsets `(0, 0)`) are skipped during alignment.

use std::path::Path;
use std::sync::Mutex;

use ndarray::Array2;
use ort::session::Session;
use ort::value::TensorRef;
use serde::Serialize;
use tokenizers::Tokenizer;

use crate::CompressFixture;

#[derive(Debug, Serialize)]
pub struct BertScoreReport {
    pub fixture: String,
    pub model: String,
    pub samples: Vec<BertScoreSampleReport>,
    pub mean_f1: f64,
    pub mean_precision: f64,
    pub mean_recall: f64,
}

#[derive(Debug, Serialize)]
pub struct BertScoreSampleReport {
    pub id: String,
    pub original_chars: usize,
    pub compressed_chars: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    pub compressed: String,
}

pub struct BertScoreScorer {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    model_label: String,
}

impl BertScoreScorer {
    pub fn new(
        model_path: impl AsRef<Path>,
        tokenizer_path: impl AsRef<Path>,
    ) -> anyhow::Result<Self> {
        let model = model_path.as_ref();
        let tok = tokenizer_path.as_ref();
        if !model.exists() {
            anyhow::bail!("bertscore model not found: {}", model.display());
        }
        if !tok.exists() {
            anyhow::bail!("tokenizer.json not found: {}", tok.display());
        }
        let mut builder = Session::builder()?;
        let session = builder.commit_from_file(model)?;
        let tokenizer =
            Tokenizer::from_file(tok).map_err(|e| anyhow::anyhow!("tokenizer load: {e}"))?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            model_label: model.display().to_string(),
        })
    }

    fn embed(&self, text: &str) -> anyhow::Result<Vec<Vec<f32>>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&u| u as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&u| u as i64)
            .collect();
        let offsets = encoding.get_offsets();
        let seq_len = ids.len();
        if seq_len == 0 {
            return Ok(Vec::new());
        }
        let id_array = Array2::from_shape_vec((1, seq_len), ids)?;
        let mask_array = Array2::from_shape_vec((1, seq_len), mask)?;
        let id_view = TensorRef::from_array_view(&id_array)?;
        let mask_view = TensorRef::from_array_view(&mask_array)?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("bertscore session poisoned"))?;
        let outputs = session.run(ort::inputs![
            "input_ids" => id_view,
            "attention_mask" => mask_view
        ])?;
        let first = outputs
            .iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("bertscore: no outputs"))?
            .1;
        let (shape, data) = first.try_extract_tensor::<f32>()?;
        let dims: Vec<i64> = shape.iter().copied().collect();
        let hidden = match dims.as_slice() {
            [1, s, h] if (*s as usize) == seq_len => *h as usize,
            other => anyhow::bail!("unexpected encoder output shape: {other:?}"),
        };
        let mut out = Vec::with_capacity(seq_len);
        for (i, off) in offsets.iter().enumerate() {
            if off.0 == off.1 {
                continue;
            }
            let start = i * hidden;
            let vec: Vec<f32> = data[start..start + hidden].to_vec();
            out.push(l2_normalise(vec));
        }
        Ok(out)
    }

    /// Returns `(precision, recall, f1)`. All in `[0, 1]`. Empty inputs
    /// return all zeros.
    pub fn score(&self, reference: &str, hypothesis: &str) -> anyhow::Result<(f64, f64, f64)> {
        let r = self.embed(reference)?;
        let h = self.embed(hypothesis)?;
        if r.is_empty() || h.is_empty() {
            return Ok((0.0, 0.0, 0.0));
        }
        // For each hypothesis token, best cosine against any reference token → P.
        let precision = mean(h.iter().map(|hv| {
            r.iter()
                .map(|rv| cosine(hv, rv))
                .fold(f32::NEG_INFINITY, f32::max)
        }));
        // For each reference token, best cosine against any hypothesis → R.
        let recall = mean(r.iter().map(|rv| {
            h.iter()
                .map(|hv| cosine(hv, rv))
                .fold(f32::NEG_INFINITY, f32::max)
        }));
        let p = precision.clamp(0.0, 1.0) as f64;
        let r_val = recall.clamp(0.0, 1.0) as f64;
        let f1 = if p + r_val == 0.0 {
            0.0
        } else {
            2.0 * p * r_val / (p + r_val)
        };
        Ok((p, r_val, f1))
    }

    pub fn evaluate_fixture(
        &self,
        fixture: &CompressFixture,
        level: rtrt_core::CompressionLevel,
    ) -> anyhow::Result<BertScoreReport> {
        let compressor = rtrt_compress::Compressor::new(level);
        let mut samples = Vec::with_capacity(fixture.samples.len());
        let mut p_sum = 0.0;
        let mut r_sum = 0.0;
        let mut f_sum = 0.0;
        for s in &fixture.samples {
            let compressed = compressor.compress(&s.body);
            let (p, r, f) = self.score(&s.body, &compressed)?;
            p_sum += p;
            r_sum += r;
            f_sum += f;
            samples.push(BertScoreSampleReport {
                id: s.id.clone(),
                original_chars: s.body.chars().count(),
                compressed_chars: compressed.chars().count(),
                precision: p,
                recall: r,
                f1: f,
                compressed,
            });
        }
        let n = fixture.samples.len() as f64;
        Ok(BertScoreReport {
            fixture: fixture.name.clone(),
            model: self.model_label.clone(),
            samples,
            mean_precision: if n == 0.0 { 0.0 } else { p_sum / n },
            mean_recall: if n == 0.0 { 0.0 } else { r_sum / n },
            mean_f1: if n == 0.0 { 0.0 } else { f_sum / n },
        })
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // Vectors are L2-normalised on embed, so cosine reduces to dot product.
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn l2_normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn mean(values: impl IntoIterator<Item = f32>) -> f32 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for v in values {
        if v.is_finite() {
            sum += v;
            count += 1;
        }
    }
    if count == 0 { 0.0 } else { sum / count as f32 }
}
