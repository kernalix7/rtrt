use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CompressionLevel, Error, Result};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub auto_compress: AutoCompressConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
}

/// Dense-embedding knobs. When `enabled = true`, the dashboard and CLI route
/// `/api/memory/recall mode=hybrid` through a real OllamaEmbedder instead of
/// the graph-blend BM25 path. The embedder uses `model` served at `base_url`.
///
/// Resolution order (highest priority first):
///   `RTRT_EMBED_ENABLED` / `RTRT_EMBED_MODEL` / `RTRT_EMBED_BASE_URL`
///   → `[embeddings]` in `~/.rtrt/config.toml`
///   → built-in defaults (disabled, bge-m3, 127.0.0.1:11434)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    /// Enable dense-vector paths. Off by default so the binary builds and runs
    /// without an Ollama instance.
    #[serde(default)]
    pub enabled: bool,
    /// Ollama model to use for embeddings (default: `bge-m3`, 1024-dim).
    #[serde(default = "default_embed_model_ollama")]
    pub model: String,
    /// Ollama base URL. `None` falls back to `auto_compress.base_url`, then
    /// `http://127.0.0.1:11434`. A trailing `/v1` is stripped so the same URL
    /// can serve both the OpenAI-compat chat path and the embeddings path.
    #[serde(default)]
    pub base_url: Option<String>,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_embed_model_ollama(),
            base_url: None,
        }
    }
}

fn default_embed_model_ollama() -> String {
    "bge-m3".to_string()
}

impl EmbeddingsConfig {
    /// Resolve the effective base URL. Priority: `RTRT_EMBED_BASE_URL` env var
    /// → `self.base_url` → `compress_base_url` fallback → Ollama default.
    pub fn resolved_base_url(&self, compress_base_url: Option<&str>) -> String {
        if let Ok(url) = std::env::var("RTRT_EMBED_BASE_URL") {
            if !url.is_empty() {
                return url;
            }
        }
        if let Some(url) = &self.base_url {
            if !url.is_empty() {
                return url.clone();
            }
        }
        if let Some(url) = compress_base_url {
            if !url.is_empty() {
                return url.to_string();
            }
        }
        "http://127.0.0.1:11434".to_string()
    }

    /// Whether embeddings are enabled, honouring the `RTRT_EMBED_ENABLED` env
    /// var first.
    pub fn is_enabled(&self) -> bool {
        match std::env::var("RTRT_EMBED_ENABLED").as_deref() {
            Ok("0") | Ok("false") | Ok("no") => false,
            Ok(v) if !v.is_empty() => true,
            _ => self.enabled,
        }
    }

    /// Effective model name, honouring `RTRT_EMBED_MODEL` env var first.
    pub fn effective_model(&self) -> String {
        std::env::var("RTRT_EMBED_MODEL").unwrap_or_else(|_| self.model.clone())
    }
}

/// Auto-capture pipeline knobs. Mirror the `RTRT_AUTO_*` env vars; env
/// always wins over the file so a one-off `RTRT_AUTO_CAPTURE=0 rtrt …`
/// still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub redact: bool,
    #[serde(default = "default_dedup_window")]
    pub dedup_window_sec: i64,
    #[serde(default)]
    pub project: Option<String>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            redact: true,
            dedup_window_sec: default_dedup_window(),
            project: None,
        }
    }
}

fn default_dedup_window() -> i64 {
    300
}

/// LLM auto-compress knobs (SessionEnd hook + dashboard daemon). Mirror the
/// `RTRT_AUTO_COMPRESS_*` env vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoCompressConfig {
    /// Off by default; set true (or `RTRT_AUTO_COMPRESS_LLM=1`) to enable.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_compress_model")]
    pub model: String,
    /// OpenAI-compatible base URL (e.g. a local Ollama endpoint).
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_compress_interval")]
    pub interval_sec: u64,
    #[serde(default = "default_compress_age")]
    pub age_sec: i64,
    #[serde(default = "default_compress_min_chars")]
    pub min_chars: usize,
    #[serde(default = "default_compress_batch")]
    pub batch: usize,
    #[serde(default = "default_compress_max_tokens")]
    pub max_tokens: u32,
}

impl Default for AutoCompressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_compress_model(),
            base_url: None,
            interval_sec: default_compress_interval(),
            age_sec: default_compress_age(),
            min_chars: default_compress_min_chars(),
            batch: default_compress_batch(),
            max_tokens: default_compress_max_tokens(),
        }
    }
}

fn default_compress_model() -> String {
    "claude-haiku-4-5".to_string()
}
fn default_compress_interval() -> u64 {
    1800
}
fn default_compress_age() -> i64 {
    3600
}
fn default_compress_min_chars() -> usize {
    // Default to "compress everything": every row is attempted once. The
    // no-shrink guard tags rows the model can't shrink with
    // `compressed_skip=no-shrink` (and `compressed_at`), so they are
    // excluded from future sweeps — each row costs at most one LLM call
    // over its lifetime. Raise this if you want to spend calls only on
    // longer rows (the bench shows ~1000+ chars is where big savings are).
    1
}
fn default_compress_batch() -> usize {
    20
}
fn default_compress_max_tokens() -> u32 {
    512
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionConfig {
    #[serde(default)]
    pub level: CompressionLevel,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            level: CompressionLevel::default(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_path")]
    pub path: PathBuf,
    #[serde(default = "default_embed_model")]
    pub embed_model: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            path: default_memory_path(),
            embed_model: default_embed_model(),
        }
    }
}

fn default_memory_path() -> PathBuf {
    PathBuf::from(".rtrt/memory.sqlite")
}

fn default_embed_model() -> String {
    "all-MiniLM-L6-v2".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    #[serde(default = "default_dashboard_addr")]
    pub bind: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            bind: default_dashboard_addr(),
        }
    }
}

fn default_dashboard_addr() -> String {
    "127.0.0.1:7311".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub active: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(format!("config TOML: {e}")))
    }

    /// Resolve the config file path: `$RTRT_CONFIG` if set, else
    /// `~/.rtrt/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("RTRT_CONFIG") {
            return Some(PathBuf::from(p));
        }
        dirs::home_dir().map(|h| h.join(".rtrt").join("config.toml"))
    }

    /// Load from the default path. Returns `Config::default()` when the file
    /// is absent; surfaces an error only on a malformed file so a typo
    /// doesn't silently fall back to defaults.
    pub fn load() -> Result<Self> {
        match Self::default_path() {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(&p)
                    .map_err(|e| Error::Config(format!("read {}: {e}", p.display())))?;
                Self::from_toml_str(&raw)
            }
            _ => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_all_defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert!(c.capture.enabled);
        assert_eq!(c.capture.dedup_window_sec, 300);
        assert!(!c.auto_compress.enabled);
        assert_eq!(c.auto_compress.model, "claude-haiku-4-5");
        assert_eq!(c.auto_compress.min_chars, 1);
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let c = Config::from_toml_str(
            r#"
            [auto_compress]
            enabled = true
            model = "gemma3:4b"
            base_url = "http://127.0.0.1:11434/v1"
            min_chars = 256
            "#,
        )
        .unwrap();
        assert!(c.auto_compress.enabled);
        assert_eq!(c.auto_compress.model, "gemma3:4b");
        assert_eq!(
            c.auto_compress.base_url.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
        assert_eq!(c.auto_compress.min_chars, 256);
        // unset field keeps its default
        assert_eq!(c.auto_compress.age_sec, 3600);
        // unrelated section still defaults
        assert!(c.capture.enabled);
    }

    #[test]
    fn malformed_toml_errors() {
        assert!(Config::from_toml_str("[auto_compress\nmodel =").is_err());
    }
}
