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
            let owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();
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
    fn blob_roundtrip() {
        let v = vec![0.1f32, -0.2, 1e9, 0.0];
        let b = vector_to_blob(&v);
        let back = vector_from_blob(&b).unwrap();
        assert_eq!(v, back);
    }
}
