//! LLMLingua-style compression scaffolding.
//!
//! Real LLMLingua ranks tokens by perplexity under a small auxiliary LM
//! (`microsoft/llmlingua-2`, GPT-2-small, …) and prunes the lowest-impact
//! tokens. Wiring an ONNX-runtime model into RTRT is multi-week work —
//! download size, tokeniser parity, GPU/CPU branch — so this module ships
//! the **interface** today and a deterministic placeholder backend so the
//! plumbing (CLI flag, MCP tool argument, tests) can land first.
//!
//! Replace [`StubMlCompressor`] with an ONNX-backed scorer when the
//! `ml-compress` feature graduates from `scaffold` to `real`.
//!
//! The token budget contract: callers ask for `target_ratio` ∈ (0, 1] and
//! the compressor returns text whose token count is approximately that
//! fraction of the input.

use rtrt_core::{Error, Result};

/// Ratio target. `0.4` keeps roughly 40% of the input tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressionTarget {
    pub ratio: f32,
}

impl CompressionTarget {
    pub fn new(ratio: f32) -> Result<Self> {
        if !(ratio.is_finite() && (0.05..=1.0).contains(&ratio)) {
            return Err(Error::Config(format!(
                "compression ratio must be in (0.05, 1.0], got {ratio}"
            )));
        }
        Ok(Self { ratio })
    }
}

/// Pluggable token-importance scorer. Returns a score in `[0, 1]` per
/// whitespace-tokenised word — higher means more salient.
pub trait TokenImportance: Send + Sync {
    fn score(&self, tokens: &[&str]) -> Vec<f32>;
    fn name(&self) -> &'static str;
}

/// Placeholder scorer that uses heuristics instead of a real LM. Useful as a
/// drop-in until the ONNX integration lands.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicImportance;

impl TokenImportance for HeuristicImportance {
    fn name(&self) -> &'static str {
        "heuristic"
    }
    fn score(&self, tokens: &[&str]) -> Vec<f32> {
        tokens
            .iter()
            .map(|t| {
                let len = t.chars().count() as f32;
                let has_digit = t.chars().any(|c| c.is_ascii_digit()) as i32 as f32;
                let has_upper = t.chars().any(|c| c.is_ascii_uppercase()) as i32 as f32;
                let punct_only = t.chars().all(|c| !c.is_alphanumeric()) as i32 as f32;
                // Longer / mixed-case / digit-bearing tokens score higher;
                // pure punctuation drops to zero.
                ((len / 10.0).clamp(0.0, 1.0) + has_digit * 0.3 + has_upper * 0.2)
                    * (1.0 - punct_only)
            })
            .collect()
    }
}

pub struct MlCompressor {
    scorer: Box<dyn TokenImportance>,
}

impl MlCompressor {
    pub fn new(scorer: Box<dyn TokenImportance>) -> Self {
        Self { scorer }
    }

    pub fn heuristic() -> Self {
        Self::new(Box::new(HeuristicImportance))
    }

    pub fn scorer_name(&self) -> &'static str {
        self.scorer.name()
    }

    /// Compresses `input` to roughly `target.ratio` of its token count.
    /// Returns the compressed text — token order is preserved; only
    /// low-importance tokens are dropped.
    pub fn compress(&self, input: &str, target: CompressionTarget) -> String {
        let tokens: Vec<&str> = input.split_whitespace().collect();
        if tokens.is_empty() {
            return String::new();
        }
        let target_count = ((tokens.len() as f32) * target.ratio).ceil().max(1.0) as usize;
        if target_count >= tokens.len() {
            return input.to_string();
        }
        let scores = self.scorer.score(&tokens);
        let mut indexed: Vec<(usize, f32)> = scores.into_iter().enumerate().collect();
        indexed.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut keep = vec![false; tokens.len()];
        for &(idx, _) in indexed.iter().take(target_count) {
            keep[idx] = true;
        }
        let mut out: Vec<&str> = Vec::with_capacity(target_count);
        for (i, t) in tokens.iter().enumerate() {
            if keep[i] {
                out.push(t);
            }
        }
        out.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_bounds() {
        assert!(CompressionTarget::new(0.5).is_ok());
        assert!(CompressionTarget::new(0.0).is_err());
        assert!(CompressionTarget::new(1.5).is_err());
    }

    #[test]
    fn drops_low_importance_tokens() {
        let c = MlCompressor::heuristic();
        let target = CompressionTarget::new(0.5).unwrap();
        let out = c.compress(
            "the parser failed on input ERROR42 due to a missing comma",
            target,
        );
        assert!(out.contains("ERROR42"), "{out}");
        // The heuristic should keep mixed-case / digit-bearing tokens before
        // bare articles/prepositions.
        assert!(!out.split_whitespace().count().eq(&0));
    }

    #[test]
    fn full_ratio_returns_input_unchanged() {
        let c = MlCompressor::heuristic();
        let target = CompressionTarget::new(1.0).unwrap();
        let input = "alpha beta gamma";
        assert_eq!(c.compress(input, target), input);
    }
}
