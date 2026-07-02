use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
    pub agents: AgentsConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub auto_compress: AutoCompressConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

/// Global security defaults applied before any per-project binding. A project
/// without its own `security_profile` (and any ad-hoc scan) falls back to
/// `default_profile`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Profile name used when a project has no bound profile. Defaults to
    /// `ai-default`.
    #[serde(default = "default_security_profile")]
    pub default_profile: String,
}

fn default_security_profile() -> String {
    "ai-default".to_string()
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            default_profile: default_security_profile(),
        }
    }
}

/// A registered project. Either a real repo on disk (`path` set) or a
/// memory-only project (`path = None`). `security_profile` binds the project
/// to a named profile; `None` means fall back to `ai-default`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    /// Absolute repo path; `None` = memory-only project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Bound profile name; `None` = use ai-default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
    /// Per-project embedding override: `Some(true)`/`Some(false)` forces the
    /// semantic (vector) memory map on/off for this project; `None` inherits the
    /// global `[embeddings] enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_enabled: Option<bool>,
}

/// Per-project customization overrides, layered on top of the global config.
/// Stored at `<repo>/.rtrt/config.toml`. Only the customization layer is
/// overridable here — the base kernel (hooks / MCP / statusLine command
/// binding) stays global and immutable except via `rtrt setup`. Every field is
/// optional: an absent field inherits the global default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Terse output level override: `off` | `lite` | `full` | `ultra`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_level: Option<String>,
    /// Output-compression override (level + enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionConfig>,
    /// Per-project agent enable/disable overlay (merged over global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<AgentsConfig>,
    /// Per-project provider enable/disable + active overlay (merged over global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<ProvidersConfig>,
    /// Opaque statusline override; shape owned by the dashboard schema so the
    /// core does not need to know it. Stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<toml::Value>,
}

impl ProjectConfig {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(format!("project config TOML: {e}")))
    }

    /// The per-project statusline override serialized as a `[statusline]` TOML
    /// section, if the project set one (the "Custom" mode). `None` means the
    /// project follows the global statusline (the default). Returned as text so
    /// callers can reuse their existing `[statusline]` parser without depending
    /// on the `toml` crate.
    pub fn statusline_section_toml(&self) -> Option<String> {
        let value = self.statusline.as_ref()?;
        let body = toml::to_string(value).ok()?;
        Some(format!("[statusline]\n{body}"))
    }

    /// True when no override is set — used to delete the file and keep the repo
    /// clean rather than leave an empty `.rtrt/config.toml`.
    pub fn is_empty(&self) -> bool {
        self.output_level.is_none()
            && self.compression.is_none()
            && self.agents.as_ref().is_none_or(|a| a.enabled.is_empty())
            && self.providers.as_ref().is_none_or(|p| {
                p.enabled.is_empty() && p.active.is_none() && p.api_max_tokens.is_none()
            })
            && self.statusline.is_none()
    }
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
    /// When embeddings are enabled, also run the background auto-embed daemon
    /// that incrementally embeds newly captured rows (so the automatic recall
    /// loop can go hybrid without a manual backfill). Defaults to `true`; set
    /// `false` to keep embeddings enabled for manual/on-demand paths only.
    #[serde(default = "default_true")]
    pub auto: bool,
    /// Seconds between auto-embed daemon sweeps. Defaults to 120.
    #[serde(default = "default_auto_embed_interval")]
    pub auto_interval_sec: u64,
    /// Max rows embedded per auto-embed sweep (one batched, capped pass per
    /// cycle so capture is never blocked). Defaults to 64.
    #[serde(default = "default_auto_embed_batch")]
    pub auto_batch: usize,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: default_embed_model_ollama(),
            base_url: None,
            auto: true,
            auto_interval_sec: default_auto_embed_interval(),
            auto_batch: default_auto_embed_batch(),
        }
    }
}

fn default_embed_model_ollama() -> String {
    "bge-m3".to_string()
}

fn default_auto_embed_interval() -> u64 {
    120
}

fn default_auto_embed_batch() -> usize {
    64
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
    default_memory_store_path()
}

/// Canonical default memory store: `~/.rtrt/memory.sqlite`.
///
/// Every surface (CLI, MCP server, dashboard, hooks, services) must resolve
/// the store through this function when no explicit `--store` /
/// `RTRT_MEMORY_PATH` override is given, so a fresh install reads and writes
/// one SQLite file instead of scattering cwd-relative stores per directory.
pub fn default_memory_store_path() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rtrt")
        .join("memory.sqlite")
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
pub struct AgentsConfig {
    #[serde(flatten)]
    pub enabled: BTreeMap<String, bool>,
}

impl AgentsConfig {
    pub fn enabled_override(&self, name: &str) -> Option<bool> {
        self.enabled.get(name).copied()
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(name.to_string(), enabled);
    }
}

/// Default output-token ceiling for routed API-mode invocations when neither
/// `[providers] api_max_tokens` nor `RTRT_API_MAX_TOKENS` is set.
pub const DEFAULT_API_MAX_TOKENS: u32 = 4096;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub active: Option<String>,
    /// Max output tokens for routed API-mode invocations (`rtrt route` /
    /// `rtrt call --mode api`, MCP `agent_call` / `agent_route`). `None`
    /// falls back to [`DEFAULT_API_MAX_TOKENS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_max_tokens: Option<u32>,
    #[serde(flatten)]
    pub enabled: BTreeMap<String, bool>,
}

impl ProvidersConfig {
    pub fn enabled_override(&self, name: &str) -> Option<bool> {
        self.enabled.get(name).copied()
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(name.to_string(), enabled);
    }

    /// Effective output-token ceiling for API-mode invocations. Resolution
    /// order mirrors the other provider knobs: `RTRT_API_MAX_TOKENS` env var
    /// → `[providers] api_max_tokens` → [`DEFAULT_API_MAX_TOKENS`]. Zero and
    /// unparseable values are ignored so a typo never truncates answers to 0.
    pub fn effective_api_max_tokens(&self) -> u32 {
        if let Ok(raw) = std::env::var("RTRT_API_MAX_TOKENS")
            && let Ok(value) = raw.trim().parse::<u32>()
            && value > 0
        {
            return value;
        }
        self.api_max_tokens
            .filter(|&value| value > 0)
            .unwrap_or(DEFAULT_API_MAX_TOKENS)
    }
}

/// Optional daily usage ceilings by provider or target name.
///
/// Example `~/.rtrt/config.toml`:
///
/// ```toml
/// [limits.openai]
/// daily_tokens = 1_000_000
/// daily_requests = 2_000
///
/// [limits.ollama]
/// daily_tokens = 250_000
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(flatten)]
    pub targets: BTreeMap<String, TargetLimit>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_requests: Option<u64>,
}

impl LimitsConfig {
    pub fn target(&self, name: &str) -> Option<&TargetLimit> {
        self.targets.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
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

    /// Look up a registered project by name.
    pub fn project(&self, name: &str) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Insert or replace a project entry, matching on `name`.
    pub fn upsert_project(&mut self, entry: ProjectEntry) {
        if let Some(existing) = self.projects.iter_mut().find(|p| p.name == entry.name) {
            *existing = entry;
        } else {
            self.projects.push(entry);
        }
    }

    pub fn set_agent_enabled(&mut self, name: &str, enabled: bool) {
        self.agents.set_enabled(name, enabled);
    }

    pub fn set_provider_enabled(&mut self, name: &str, enabled: bool) {
        self.providers.set_enabled(name, enabled);
    }

    pub fn set_tool_enabled(&mut self, name: &str, enabled: bool) {
        if self.providers.enabled.contains_key(name) {
            self.set_provider_enabled(name, enabled);
        } else {
            self.set_agent_enabled(name, enabled);
        }
    }

    /// Per-project override file: `<repo>/.rtrt/config.toml`.
    pub fn project_config_path(repo: &Path) -> PathBuf {
        repo.join(".rtrt").join("config.toml")
    }

    /// Load a project's override file if present (empty default otherwise).
    pub fn load_project(repo: &Path) -> Result<ProjectConfig> {
        match std::fs::read_to_string(Self::project_config_path(repo)) {
            Ok(raw) => ProjectConfig::from_toml_str(&raw),
            Err(_) => Ok(ProjectConfig::default()),
        }
    }

    /// Load the global config and overlay a project's customization overrides.
    /// The base kernel is never overlaid — only the customization layer
    /// (output level, compression, enabled agents/providers).
    pub fn load_effective(repo: Option<&Path>) -> Result<Self> {
        let mut base = Self::load()?;
        if let Some(repo) = repo {
            let over = Self::load_project(repo)?;
            base.apply_project_overrides(&over);
        }
        Ok(base)
    }

    /// The config effective for the current working directory: the global
    /// config overlaid with the enclosing repo's `.rtrt/config.toml` when the
    /// cwd is inside a repo, else the plain global config. Errors fall back to
    /// the default config so a malformed per-project file never breaks the
    /// caller (routing, MCP tool dispatch, hooks).
    pub fn load_effective_for_cwd() -> Self {
        let repo = std::env::current_dir()
            .ok()
            .and_then(|cwd| repo_root_from(&cwd));
        Self::load_effective(repo.as_deref()).unwrap_or_default()
    }

    /// Overlay one project's customization overrides onto this config.
    pub fn apply_project_overrides(&mut self, over: &ProjectConfig) {
        if let Some(compression) = &over.compression {
            self.compression = compression.clone();
        }
        if let Some(agents) = &over.agents {
            for (name, enabled) in &agents.enabled {
                self.agents.enabled.insert(name.clone(), *enabled);
            }
        }
        if let Some(providers) = &over.providers {
            for (name, enabled) in &providers.enabled {
                self.providers.enabled.insert(name.clone(), *enabled);
            }
            if providers.active.is_some() {
                self.providers.active = providers.active.clone();
            }
            if providers.api_max_tokens.is_some() {
                self.providers.api_max_tokens = providers.api_max_tokens;
            }
        }
    }

    /// Write a project override file, creating `.rtrt/` as needed. When the
    /// override is empty the file is removed so the repo stays clean.
    pub fn save_project(repo: &Path, over: &ProjectConfig) -> Result<()> {
        let path = Self::project_config_path(repo);
        if over.is_empty() {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        let body = toml::to_string_pretty(over)
            .map_err(|e| Error::Config(format!("serialize project config: {e}")))?;
        std::fs::write(&path, body)
            .map_err(|e| Error::Config(format!("write {}: {e}", path.display())))?;
        Ok(())
    }
}

/// Walk up from `start` to the enclosing repo root — the first ancestor with a
/// `.git` or `.rtrt` entry. Returns `None` when `start` is not inside a repo,
/// so callers fall back to the plain global config.
pub fn repo_root_from(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() || dir.join(".rtrt").exists() {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
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
    fn embeddings_auto_defaults_and_old_configs_load() {
        // Empty config: auto-embed daemon knobs take their defaults.
        let c = Config::from_toml_str("").unwrap();
        assert!(!c.embeddings.enabled);
        assert!(c.embeddings.auto);
        assert_eq!(c.embeddings.auto_interval_sec, 120);
        assert_eq!(c.embeddings.auto_batch, 64);

        // An "old" [embeddings] block that predates the daemon knobs must still
        // load (serde default) and fill in the new fields.
        let c = Config::from_toml_str(
            r#"
            [embeddings]
            enabled = true
            model = "nomic-embed-text"
            "#,
        )
        .unwrap();
        assert!(c.embeddings.enabled);
        assert_eq!(c.embeddings.model, "nomic-embed-text");
        assert!(c.embeddings.auto);
        assert_eq!(c.embeddings.auto_interval_sec, 120);
        assert_eq!(c.embeddings.auto_batch, 64);

        // Explicit overrides win.
        let c = Config::from_toml_str(
            r#"
            [embeddings]
            enabled = true
            auto = false
            auto_interval_sec = 300
            auto_batch = 16
            "#,
        )
        .unwrap();
        assert!(!c.embeddings.auto);
        assert_eq!(c.embeddings.auto_interval_sec, 300);
        assert_eq!(c.embeddings.auto_batch, 16);
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

    #[test]
    fn agent_and_provider_detect_overrides_load() {
        let c = Config::from_toml_str(
            r#"
            [agents]
            claude = true
            aider = false

            [providers]
            active = "openai"
            openrouter = false
            "#,
        )
        .unwrap();
        assert_eq!(c.agents.enabled_override("claude"), Some(true));
        assert_eq!(c.agents.enabled_override("aider"), Some(false));
        assert_eq!(c.agents.enabled_override("codex"), None);
        assert_eq!(c.providers.active.as_deref(), Some("openai"));
        assert_eq!(c.providers.enabled_override("openrouter"), Some(false));
    }

    #[test]
    fn limits_load_as_target_tables() {
        let c = Config::from_toml_str(
            r#"
            [limits.openai]
            daily_tokens = 1_000_000
            daily_requests = 2_000

            [limits.ollama]
            daily_tokens = 250_000
            "#,
        )
        .unwrap();

        let openai = c.limits.target("openai").unwrap();
        assert_eq!(openai.daily_tokens, Some(1_000_000));
        assert_eq!(openai.daily_requests, Some(2_000));
        let ollama = c.limits.target("ollama").unwrap();
        assert_eq!(ollama.daily_tokens, Some(250_000));
        assert_eq!(ollama.daily_requests, None);
    }

    #[test]
    fn upsert_replaces_by_name_no_dup() {
        let mut c = Config::default();
        c.upsert_project(ProjectEntry {
            name: "alpha".to_string(),
            path: Some("/repo/alpha".to_string()),
            security_profile: None,
            embeddings_enabled: None,
        });
        c.upsert_project(ProjectEntry {
            name: "beta".to_string(),
            path: None,
            security_profile: Some("strict".to_string()),
            embeddings_enabled: None,
        });
        // replace alpha
        c.upsert_project(ProjectEntry {
            name: "alpha".to_string(),
            path: Some("/repo/alpha-2".to_string()),
            security_profile: Some("ai-default".to_string()),
            embeddings_enabled: None,
        });
        assert_eq!(c.projects.len(), 2);
        let alpha = c.project("alpha").unwrap();
        assert_eq!(alpha.path.as_deref(), Some("/repo/alpha-2"));
        assert_eq!(alpha.security_profile.as_deref(), Some("ai-default"));
    }

    #[test]
    fn api_max_tokens_loads_and_defaults() {
        // Absent → the safe default (no silent truncation to a tiny cap).
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.providers.api_max_tokens, None);
        assert_eq!(
            c.providers.effective_api_max_tokens(),
            DEFAULT_API_MAX_TOKENS
        );

        // Explicit value in [providers] wins; sibling flattened bool entries
        // (the enable map) must keep loading next to the typed field.
        let c = Config::from_toml_str(
            r#"
            [providers]
            active = "openai"
            api_max_tokens = 8192
            openrouter = false
            "#,
        )
        .unwrap();
        assert_eq!(c.providers.api_max_tokens, Some(8192));
        assert_eq!(c.providers.effective_api_max_tokens(), 8192);
        assert_eq!(c.providers.enabled_override("openrouter"), Some(false));

        // Zero is ignored — a typo must never truncate answers to nothing.
        let zeroed = ProvidersConfig {
            api_max_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(zeroed.effective_api_max_tokens(), DEFAULT_API_MAX_TOKENS);
    }

    #[test]
    fn project_override_carries_api_max_tokens() {
        let mut base = Config::from_toml_str(
            r#"
            [providers]
            api_max_tokens = 2048
            "#,
        )
        .unwrap();
        let over = ProjectConfig::from_toml_str(
            r#"
            [providers]
            api_max_tokens = 512
            "#,
        )
        .unwrap();
        assert!(!over.is_empty());
        base.apply_project_overrides(&over);
        assert_eq!(base.providers.api_max_tokens, Some(512));

        // An override without the field leaves the global value alone.
        let mut base = Config::from_toml_str(
            r#"
            [providers]
            api_max_tokens = 2048
            "#,
        )
        .unwrap();
        let over = ProjectConfig::from_toml_str(
            r#"
            [providers]
            active = "openai"
            "#,
        )
        .unwrap();
        base.apply_project_overrides(&over);
        assert_eq!(base.providers.api_max_tokens, Some(2048));
    }

    #[test]
    fn repo_root_walks_up_to_rtrt_or_git_marker() {
        let root = std::env::temp_dir().join(format!(
            "rtrt-core-repo-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(repo_root_from(&nested), None);
        std::fs::create_dir_all(root.join(".rtrt")).unwrap();
        assert_eq!(repo_root_from(&nested), Some(root.clone()));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn project_finds_and_none() {
        let mut c = Config::default();
        assert!(c.project("missing").is_none());
        c.upsert_project(ProjectEntry {
            name: "gamma".to_string(),
            path: None,
            security_profile: None,
            embeddings_enabled: None,
        });
        assert!(c.project("gamma").is_some());
        assert!(c.project("nope").is_none());
    }
}
