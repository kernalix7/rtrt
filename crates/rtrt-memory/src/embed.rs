//! Embedding trait and the default `fastembed`-backed implementation.
//!
//! The trait stays available in all builds so call sites can be written once.
//! The concrete [`FastEmbedder`] lives behind the `embeddings` feature because
//! it pulls in the `fastembed` crate, which downloads the ONNX model file on
//! first use.

use rtrt_core::{Error, Result};

/// Returns embeddings as `Vec<f32>` per input string. Implementations must
/// return vectors of the same length within a single call.
pub trait Embedder: Send + Sync {
    fn dimension(&self) -> usize;
    fn model_name(&self) -> &str;
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed(&[text])?;
        out.pop()
            .ok_or_else(|| Error::Memory("embedder returned no vector".into()))
    }
}

/// Max characters fed to an embedding model. Embeddings only capture the gist,
/// and the local models (nomic-embed-text, bge, MiniLM) have small context
/// windows — sending a 31k-char body either truncates server-side or times out,
/// which is why backfill historically covered only a fraction of a project.
/// Capping by *char* (not byte) keeps multi-byte text intact.
// Only the concrete embedders (and the test) consume this; allow dead_code when
// neither embedder feature is enabled so the bare `hnsw` build stays warning-free.
#[cfg_attr(
    not(any(feature = "ollama-embed", feature = "embeddings", test)),
    allow(dead_code)
)]
pub(crate) const EMBED_CHAR_CAP: usize = 2000;

/// Returns `text` capped to the first [`EMBED_CHAR_CAP`] chars. Cheap no-op when
/// the text is already short (borrows instead of allocating).
#[cfg_attr(
    not(any(feature = "ollama-embed", feature = "embeddings", test)),
    allow(dead_code)
)]
pub(crate) fn truncate_for_embed(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().count() <= EMBED_CHAR_CAP {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.chars().take(EMBED_CHAR_CAP).collect())
    }
}

/// Vec<f32> → little-endian byte BLOB (4 bytes per element). Round-trips with
/// [`vector_from_blob`].
pub fn vector_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Little-endian f32 byte BLOB → Vec<f32>. Returns an error if `blob.len() % 4 != 0`.
pub fn vector_from_blob(blob: &[u8]) -> Result<Vec<f32>> {
    if blob.len() % 4 != 0 {
        return Err(Error::Memory(format!(
            "vector blob length {} is not 4-aligned",
            blob.len()
        )));
    }
    let mut out = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Ok(out)
}

/// Cosine similarity in `[-1, 1]`. Returns `0.0` when either input is the zero vector.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(feature = "ollama-embed")]
mod ollama_impl {
    use rtrt_core::{Error, Result};

    use super::Embedder;

    /// Ollama-backed embedder. Calls `POST {base_url}/api/embeddings` for each
    /// text (Ollama accepts one string per call, so `embed` loops). The
    /// dimension is detected on first call and cached; if the probe fails it
    /// defaults to 1024 (the bge-m3 native size).
    pub struct OllamaEmbedder {
        base_url: String,
        model: String,
        // `None` until the first successful embed; then locked in forever.
        dim: std::sync::OnceLock<usize>,
    }

    impl OllamaEmbedder {
        /// Constructs an embedder pointing at `base_url` (e.g.
        /// `http://127.0.0.1:11434`). A trailing `/v1` is stripped so the
        /// same URL that works for OpenAI-compat chat routes to the correct
        /// `/api/embeddings` path.
        pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
            let mut url = base_url.into();
            // Strip a trailing `/v1` — the embeddings endpoint lives at the
            // host root, not under the OpenAI-compat prefix.
            if url.ends_with("/v1") {
                url.truncate(url.len() - 3);
            }
            url = url.trim_end_matches('/').to_string();
            Self {
                base_url: url,
                model: model.into(),
                dim: std::sync::OnceLock::new(),
            }
        }

        fn embed_one_text(&self, text: &str) -> Result<Vec<f32>> {
            let url = format!("{}/api/embeddings", self.base_url);
            // Cap the prompt: long bodies otherwise time out or fail at the
            // model's context window, leaving rows unembedded.
            let prompt = super::truncate_for_embed(text);
            let body = serde_json::json!({
                "model": self.model,
                "prompt": prompt.as_ref(),
            });
            let resp = ureq::post(&url)
                .set("Content-Type", "application/json")
                .send_json(&body)
                .map_err(|e| Error::Memory(format!("ollama embeddings request: {e}")))?;
            let v: serde_json::Value = resp
                .into_json()
                .map_err(|e| Error::Memory(format!("ollama embeddings decode: {e}")))?;
            let arr = v
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| Error::Memory("ollama: missing `embedding` array".into()))?;
            let vec: Vec<f32> = arr
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            if vec.is_empty() {
                return Err(Error::Memory("ollama: empty embedding vector".into()));
            }
            Ok(vec)
        }
    }

    impl Embedder for OllamaEmbedder {
        fn dimension(&self) -> usize {
            // Return whatever was detected on the first successful call, or
            // fall back to the bge-m3 default of 1024.
            *self.dim.get().unwrap_or(&1024)
        }

        fn model_name(&self) -> &str {
            &self.model
        }

        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            let mut out = Vec::with_capacity(texts.len());
            for &text in texts {
                let vec = self.embed_one_text(text)?;
                // Latch the dimension on the first successful call.
                let _ = self.dim.set(vec.len());
                out.push(vec);
            }
            Ok(out)
        }
    }
}

#[cfg(feature = "ollama-embed")]
pub use ollama_impl::OllamaEmbedder;

#[cfg(feature = "embeddings")]
mod fastembed_impl {
    use std::sync::Mutex;

    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use rtrt_core::{Error, Result};

    use super::Embedder;

    /// Default `all-MiniLM-L6-v2` embedder (384-dim, offline after first download).
    pub struct FastEmbedder {
        model: Mutex<TextEmbedding>,
        dimension: usize,
        name: String,
    }

    impl FastEmbedder {
        /// Constructs the default model (`AllMiniLML6V2`, 384-dim).
        pub fn new_default() -> Result<Self> {
            let opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2);
            let model = TextEmbedding::try_new(opts)
                .map_err(|e| Error::Memory(format!("fastembed init: {e}")))?;
            Ok(Self {
                model: Mutex::new(model),
                dimension: 384,
                name: "all-MiniLM-L6-v2".to_string(),
            })
        }
    }

    impl Embedder for FastEmbedder {
        fn dimension(&self) -> usize {
            self.dimension
        }

        fn model_name(&self) -> &str {
            &self.name
        }

        fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            // Cap each text: MiniLM truncates anything past its 256-token window
            // anyway, and the cap keeps long bodies from blowing the budget.
            let owned: Vec<String> = texts
                .iter()
                .map(|s| super::truncate_for_embed(s).into_owned())
                .collect();
            let mut guard = self
                .model
                .lock()
                .map_err(|_| Error::Memory("embedder poisoned".into()))?;
            guard
                .embed(owned, None)
                .map_err(|e| Error::Memory(format!("fastembed embed: {e}")))
        }
    }
}

#[cfg(feature = "embeddings")]
pub use fastembed_impl::FastEmbedder;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    #[test]
    fn cosine_identical() {
        let a = vec![0.5, 0.5, 0.5];
        let v = cosine(&a, &a);
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn truncate_caps_by_char() {
        let short = "hello";
        assert_eq!(truncate_for_embed(short).as_ref(), "hello");
        // Multi-byte chars: cap counts chars, not bytes, and never splits one.
        let long: String = "あ".repeat(EMBED_CHAR_CAP + 500);
        let capped = truncate_for_embed(&long);
        assert_eq!(capped.chars().count(), EMBED_CHAR_CAP);
    }

    #[test]
    fn blob_roundtrip() {
        let v = vec![0.1f32, -0.2, 1e9, 0.0];
        let b = vector_to_blob(&v);
        let back = vector_from_blob(&b).unwrap();
        assert_eq!(v, back);
    }
}
