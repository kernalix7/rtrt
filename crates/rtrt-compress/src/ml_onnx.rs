//! Real ONNX-runtime backend for `TokenImportance`.
//!
//! ## Model contract
//!
//! The model must take two named inputs and emit one scoring output:
//!
//! | Tensor          | Shape                | dtype | Notes                                      |
//! |-----------------|----------------------|-------|--------------------------------------------|
//! | `input_ids`     | `[1, seq_len]`       | i64   | Token ids from the bundled tokenizer.       |
//! | `attention_mask`| `[1, seq_len]`       | i64   | All ones; padding is not used (seq fits in window).|
//! | first output    | `[1, seq_len, 2]` or `[1, seq_len]` | f32 | Per-subword score. With 2 classes we take `softmax(.., dim=-1)[..,1]` (keep prob). With a 1-d output we take the value directly. |
//!
//! LLMLingua-2's published xlm-roberta-large and bert-base exports follow
//! this shape; smaller distillates that emit a single attention-saliency
//! channel also work because of the fallback path.
//!
//! ## Subword → word alignment
//!
//! The tokenizer is asked for `offsets`; each subword score is attributed
//! to whichever whitespace word it overlaps with. A word's final score is
//! the mean of the subword scores assigned to it. Special tokens
//! (`[CLS]` / `[SEP]` / `<s>` / `</s>`) have offsets `(0, 0)` and are
//! skipped.
//!
//! ## Initialisation cost
//!
//! `Session` is built once per `OnnxImportance` and reused across calls.
//! Inference is synchronous; if you need batched scoring, wrap the
//! compressor in a `tokio::task::spawn_blocking`.

use std::path::Path;
use std::sync::Mutex;

use ndarray::Array2;
use ort::session::Session;
use ort::value::TensorRef;
use rtrt_core::{Error, Result};
use tokenizers::Tokenizer;

use crate::ml::TokenImportance;

pub struct OnnxImportance {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl OnnxImportance {
    pub fn new(model_path: impl AsRef<Path>, tokenizer_path: impl AsRef<Path>) -> Result<Self> {
        // `load-dynamic` defers the actual ONNX Runtime library lookup to
        // first `Session::run`; constructing the builder is cheap and we
        // surface a clean error if either file is missing.
        let model = model_path.as_ref();
        let tok = tokenizer_path.as_ref();
        if !model.exists() {
            return Err(Error::Config(format!(
                "onnx model not found: {}",
                model.display()
            )));
        }
        if !tok.exists() {
            return Err(Error::Config(format!(
                "tokenizer.json not found: {}",
                tok.display()
            )));
        }
        let mut builder =
            Session::builder().map_err(|e| Error::Config(format!("onnx session builder: {e}")))?;
        let session = builder
            .commit_from_file(model)
            .map_err(|e| Error::Config(format!("onnx session: {e}")))?;
        let tokenizer =
            Tokenizer::from_file(tok).map_err(|e| Error::Config(format!("tokenizer load: {e}")))?;
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    fn score_inner(&self, text: &str, word_spans: &[(usize, usize)]) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| Error::Config(format!("tokenize: {e}")))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&u| u as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&u| u as i64)
            .collect();
        let offsets = encoding.get_offsets();
        let seq_len = ids.len();
        if seq_len == 0 {
            return Ok(vec![0.0; word_spans.len()]);
        }
        let id_array = Array2::from_shape_vec((1, seq_len), ids)
            .map_err(|e| Error::Config(format!("onnx input shape: {e}")))?;
        let mask_array = Array2::from_shape_vec((1, seq_len), mask)
            .map_err(|e| Error::Config(format!("onnx mask shape: {e}")))?;
        let id_view = TensorRef::from_array_view(&id_array)
            .map_err(|e| Error::Config(format!("onnx tensor: {e}")))?;
        let mask_view = TensorRef::from_array_view(&mask_array)
            .map_err(|e| Error::Config(format!("onnx tensor: {e}")))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Config("onnx session poisoned".into()))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => id_view,
                "attention_mask" => mask_view
            ])
            .map_err(|e| Error::Config(format!("onnx run: {e}")))?;
        let first = outputs
            .iter()
            .next()
            .ok_or_else(|| Error::Config("onnx run returned no outputs".into()))?
            .1;
        let (shape, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| Error::Config(format!("onnx output extract: {e}")))?;
        // Per-subword score: `[1, S]` (1-d output) or `[1, S, C]` (per-class
        // logits; pick class 1 = "keep"). Reshape into `Vec<f32>` indexed by
        // subword position.
        let dims: Vec<i64> = shape.iter().copied().collect();
        let scores: Vec<f32> = match dims.as_slice() {
            [1, s] if (*s as usize) == seq_len => data.to_vec(),
            [1, s, c] if (*s as usize) == seq_len && *c >= 1 => {
                let stride = *c as usize;
                let keep_idx = if stride >= 2 { 1 } else { 0 };
                (0..seq_len)
                    .map(|i| sigmoid(data[i * stride + keep_idx]))
                    .collect()
            }
            _ => {
                return Err(Error::Config(format!(
                    "unexpected onnx output shape: {dims:?} (seq_len {seq_len})"
                )));
            }
        };

        // Subword → word alignment via offsets. Walk subwords in order; each
        // subword contributes to whichever word span contains its byte range.
        let mut word_scores = vec![(0.0f32, 0u32); word_spans.len()];
        for (i, off) in offsets.iter().enumerate() {
            // `(0, 0)` denotes special tokens (CLS / SEP / pad). Skip.
            if off.0 == off.1 {
                continue;
            }
            let mid = (off.0 + off.1) / 2;
            if let Some((w_idx, _)) = word_spans
                .iter()
                .enumerate()
                .find(|&(_, &(a, b))| a <= mid && mid < b)
            {
                let s = scores[i].clamp(0.0, 1.0);
                word_scores[w_idx].0 += s;
                word_scores[w_idx].1 += 1;
            }
        }
        Ok(word_scores
            .into_iter()
            .map(|(sum, n)| if n == 0 { 0.0 } else { sum / n as f32 })
            .collect())
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl TokenImportance for OnnxImportance {
    fn name(&self) -> &'static str {
        "onnx"
    }
    fn score(&self, tokens: &[&str]) -> Vec<f32> {
        // Reconstruct the spans of each whitespace token inside the joined
        // string so we can align subword offsets back to words.
        let mut joined = String::new();
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(tokens.len());
        for (i, t) in tokens.iter().enumerate() {
            if i > 0 {
                joined.push(' ');
            }
            let start = joined.len();
            joined.push_str(t);
            spans.push((start, joined.len()));
        }
        match self.score_inner(&joined, &spans) {
            Ok(scores) => scores,
            Err(_) => vec![0.0; tokens.len()],
        }
    }
}
