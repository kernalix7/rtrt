use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{CompressionLevel, Error, Result};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
static CONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Only permission-prompt bridge accepted for delegated Claude CLI lanes.
pub const CLAUDE_PERMISSION_PROMPT_TOOL: &str = "mcp__rtrt__permission_prompt";

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
    #[serde(default, skip_serializing_if = "FailoverConfig::is_default")]
    pub failover: FailoverConfig,
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
    /// Per-project invocation failure policy (`[failover]`). Whole-section
    /// replacement because the three marker classes are consulted in priority
    /// order, so appending a project list to a global one would silently
    /// reclassify markers the project never mentioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failover: Option<FailoverConfig>,
}

impl ProjectConfig {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let over: Self =
            toml::from_str(s).map_err(|e| Error::Config(format!("project config TOML: {e}")))?;
        Ok(over)
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
            && self.failover.is_none()
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
    /// Runtime/provider identity behind `base_url`. This is deliberately
    /// separate from its OpenAI-compatible wire protocol.
    #[serde(default)]
    pub provider: Option<String>,
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
            provider: None,
            interval_sec: default_compress_interval(),
            age_sec: default_compress_age(),
            min_chars: default_compress_min_chars(),
            batch: default_compress_batch(),
            max_tokens: default_compress_max_tokens(),
        }
    }
}

impl AutoCompressConfig {
    /// Identity for an OpenAI-compatible endpoint. Explicit env/config values
    /// win. Only Ollama's shipped loopback URL is recognised implicitly;
    /// arbitrary compatible endpoints retain the neutral identity.
    pub fn effective_provider(&self, base_url: &str) -> String {
        let provider = std::env::var("RTRT_OPENAI_COMPAT_PROVIDER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.provider
                    .clone()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| {
                if is_default_ollama_compatible_url(base_url) {
                    "ollama".to_string()
                } else {
                    "openai-compat".to_string()
                }
            });
        normalize_provider_id(&provider)
    }
}

/// Canonical provider/runtime identity used at configuration and routing
/// boundaries. Provider names are case-insensitive. These aliases are limited
/// to explicit spellings of the same runtime; transport names never become a
/// provider and collapse to the neutral compatible-endpoint identity.
pub fn normalize_provider_id(provider: &str) -> String {
    let normalized = provider.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "openai-compatible" | "openai_compatible" | "openai-compat" => "openai-compat".to_string(),
        "lmstudio" | "lm_studio" | "lms" | "lm-studio" => "lm-studio".to_string(),
        "llama" | "llamacpp" | "llama_cpp" | "llama-cpp" | "llama.cpp" => "llama.cpp".to_string(),
        _ => normalized,
    }
}

/// Whether `url` is the known default Ollama loopback endpoint. Do not extend
/// this into runtime fingerprinting: compatible servers must identify
/// themselves explicitly.
pub fn is_default_ollama_compatible_url(url: &str) -> bool {
    matches!(
        url.trim().trim_end_matches('/'),
        "http://127.0.0.1:11434"
            | "http://127.0.0.1:11434/v1"
            | "http://localhost:11434"
            | "http://localhost:11434/v1"
            | "http://[::1]:11434"
            | "http://[::1]:11434/v1"
    )
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

/// Explicit legacy/admin memory store: `~/.rtrt/memory.sqlite`.
///
/// Retained for backward-compatible configuration and explicit migration
/// workflows. New project-scoped callers must not use this default.
pub fn default_memory_store_path() -> PathBuf {
    legacy_memory_store_path()
}

/// Explicit legacy/admin path. Strict project callers must instead use
/// [`crate::project_memory_db_path`].
pub fn legacy_memory_store_path() -> PathBuf {
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
        let normalized = normalize_provider_id(name);
        self.enabled
            .iter()
            .find(|(configured, _)| normalize_provider_id(configured) == normalized)
            .map(|(_, enabled)| *enabled)
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        self.enabled.insert(normalize_provider_id(name), enabled);
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
///
/// One target often fronts several upstream quotas (see [`crate::pool`]). Those
/// pools can be capped individually, nested under the target they belong to:
///
/// ```toml
/// [limits.opencode]
/// daily_tokens = 2_000_000        # still the target-wide cap
///
/// [limits.opencode.pools.opencode-go]
/// daily_tokens = 1_200_000
///
/// [limits.opencode.pools.ollama]
/// daily_requests = 500
/// ```
///
/// Pool caps are strictly optional: a target with none behaves exactly as it
/// always has, and its pools share the target-wide cap rather than each being
/// given a synthesised slice of it.
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
    /// Per-pool ceilings inside this target (`[limits.<target>.pools.<pool>]`).
    /// Absent for every config written before pool identity existed, and
    /// skipped on serialize when empty, so those configs round-trip byte for
    /// byte.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pools: BTreeMap<String, PoolLimit>,
}

/// Daily ceilings for one upstream pool inside a target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_requests: Option<u64>,
}

impl TargetLimit {
    /// The cap configured for one pool inside this target, if any. Matched
    /// exactly first, then case-insensitively, because pool names derived from
    /// model strings are lowercased.
    pub fn pool(&self, name: &str) -> Option<&PoolLimit> {
        if let Some(limit) = self.pools.get(name) {
            return Some(limit);
        }
        self.pools
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, limit)| limit)
    }

    /// True when at least one pool inside this target carries its own cap.
    pub fn has_pool_limits(&self) -> bool {
        self.pools.values().any(PoolLimit::is_set)
    }
}

impl PoolLimit {
    /// True when this pool pins at least one axis.
    pub fn is_set(&self) -> bool {
        self.daily_tokens.is_some() || self.daily_requests.is_some()
    }
}

impl LimitsConfig {
    pub fn target(&self, name: &str) -> Option<&TargetLimit> {
        self.targets.get(name).or_else(|| {
            self.targets
                .iter()
                .find(|(target, _)| target.eq_ignore_ascii_case(name))
                .map(|(_, limit)| limit)
        })
    }

    /// The cap for one pool inside a target, if the config pins one.
    pub fn pool(&self, target: &str, pool: &str) -> Option<&PoolLimit> {
        self.target(target)?.pool(pool)
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
        let config: Self =
            toml::from_str(s).map_err(|e| Error::Config(format!("config TOML: {e}")))?;
        Ok(config)
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
                let raw = read_bounded_config(&p, false)?;
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
        let root = canonical_repo_root(repo)?;
        let Some(config_dir) = existing_project_config_dir(&root)? else {
            return Ok(ProjectConfig::default());
        };
        let path = config_dir.join("config.toml");
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                validate_project_target(&root, &path, &metadata)?;
                let raw = read_bounded_config(&path, true)?;
                ProjectConfig::from_toml_str(&raw)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProjectConfig::default())
            }
            Err(error) => Err(config_error("inspect", &path, error)),
        }
    }

    /// Load the global config and overlay a project's customization overrides.
    /// The base kernel is never overlaid — only the customization layer
    /// (output level, compression, enabled agents/providers, failover).
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
        if let Some(failover) = &over.failover {
            self.failover = failover.clone();
        }
    }

    /// Write a project override file, creating `.rtrt/` as needed. When the
    /// override is empty the file is removed so the repo stays clean.
    pub fn save_project(repo: &Path, over: &ProjectConfig) -> Result<()> {
        let root = canonical_repo_root(repo)?;
        let config_dir = prepare_project_config_dir(&root, !over.is_empty())?;
        let Some(config_dir) = config_dir else {
            return Ok(());
        };
        let path = config_dir.join("config.toml");
        let target = match fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(config_error("inspect", &path, error)),
        };
        if let Some(metadata) = &target {
            validate_project_target(&root, &path, metadata)?;
        }
        if over.is_empty() {
            if target.is_some() {
                fs::remove_file(&path).map_err(|error| config_error("remove", &path, error))?;
            }
            return Ok(());
        }
        let body = toml::to_string_pretty(over)
            .map_err(|e| Error::Config(format!("serialize project config: {e}")))?;
        if body.len() as u64 > MAX_CONFIG_BYTES {
            return Err(Error::Config(format!(
                "project config exceeds {MAX_CONFIG_BYTES} bytes"
            )));
        }
        atomic_write_project_config(&root, &config_dir, &path, body.as_bytes())
    }
}

fn config_error(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error::Config(format!("{action} {}: {error}", path.display()))
}

fn canonical_repo_root(repo: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(repo).map_err(|error| config_error("canonicalize", repo, error))?;
    let metadata = fs::metadata(&root).map_err(|error| config_error("inspect", &root, error))?;
    if !metadata.is_dir() {
        return Err(Error::Config(format!(
            "project root is not a directory: {}",
            root.display()
        )));
    }
    Ok(root)
}

fn existing_project_config_dir(root: &Path) -> Result<Option<PathBuf>> {
    let path = root.join(".rtrt");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(config_error("inspect", &path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Config(format!(
            "project config directory must be a real directory: {}",
            path.display()
        )));
    }
    validate_same_owner(root, &path, &metadata)?;
    let canonical =
        fs::canonicalize(&path).map_err(|error| config_error("canonicalize", &path, error))?;
    if canonical.parent() != Some(root) {
        return Err(Error::Config(format!(
            "project config directory escapes repository: {}",
            path.display()
        )));
    }
    Ok(Some(canonical))
}

fn prepare_project_config_dir(root: &Path, create: bool) -> Result<Option<PathBuf>> {
    if let Some(path) = existing_project_config_dir(root)? {
        return Ok(Some(path));
    }
    if !create {
        return Ok(None);
    }
    let path = root.join(".rtrt");
    create_private_directory(&path)?;
    existing_project_config_dir(root)
}

fn validate_project_target(root: &Path, path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Config(format!(
            "project config must be a regular non-symlink file: {}",
            path.display()
        )));
    }
    validate_same_owner(root, path, metadata)
}

#[cfg(unix)]
fn validate_same_owner(root: &Path, path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let root_metadata = fs::metadata(root).map_err(|error| config_error("inspect", root, error))?;
    if metadata.uid() != root_metadata.uid() {
        return Err(Error::Config(format!(
            "project config path has foreign ownership: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_same_owner(_root: &Path, _path: &Path, _metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

fn read_bounded_config(path: &Path, reject_symlink: bool) -> Result<String> {
    let expected = if reject_symlink {
        let metadata =
            fs::symlink_metadata(path).map_err(|error| config_error("inspect", path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::Config(format!(
                "config must be a regular non-symlink file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(Error::Config(format!(
                "config exceeds {MAX_CONFIG_BYTES} bytes: {}",
                path.display()
            )));
        }
        Some(metadata)
    } else {
        None
    };
    let file = File::open(path).map_err(|error| config_error("read", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| config_error("inspect", path, error))?;
    if expected
        .as_ref()
        .is_some_and(|expected| !metadata_same_file(expected, &metadata))
    {
        return Err(Error::Config(format!(
            "config changed while opening: {}",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(Error::Config(format!(
            "config is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(Error::Config(format!(
            "config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| config_error("read", path, error))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(Error::Config(format!(
            "config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        )));
    }
    String::from_utf8(bytes)
        .map_err(|error| Error::Config(format!("read {}: {error}", path.display())))
}

#[cfg(unix)]
fn metadata_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

// True file identity on Windows needs `volume_serial_number`/`file_index`, which
// are still unstable and only populated for handle-derived metadata. This
// compares every stable attribute instead: it detects the swap-and-replace this
// guards against, but two distinct files sharing all of them would compare
// equal, so it is a tamper check rather than an identity check.
#[cfg(windows)]
fn metadata_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.file_attributes() == right.file_attributes()
        && left.creation_time() == right.creation_time()
        && left.last_write_time() == right.last_write_time()
        && left.file_size() == right.file_size()
        && left.file_type() == right.file_type()
}

#[cfg(not(any(unix, windows)))]
fn metadata_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.file_type() == right.file_type() && left.len() == right.len()
}

struct TempConfig {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn atomic_write_project_config(
    root: &Path,
    config_dir: &Path,
    destination: &Path,
    body: &[u8],
) -> Result<()> {
    let mut last_error = None;
    for _ in 0..32 {
        let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = config_dir.join(format!(
            ".config.toml.tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(mut file) => {
                let mut temp = TempConfig {
                    path: temp_path,
                    armed: true,
                };
                file.write_all(body)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| config_error("write", &temp.path, error))?;
                let written_metadata = file
                    .metadata()
                    .map_err(|error| config_error("inspect", &temp.path, error))?;
                drop(file);
                let checked_dir = existing_project_config_dir(root)?.ok_or_else(|| {
                    Error::Config("project config directory disappeared during write".to_string())
                })?;
                if checked_dir != config_dir {
                    return Err(Error::Config(
                        "project config directory changed during write".to_string(),
                    ));
                }
                if let Ok(metadata) = fs::symlink_metadata(destination) {
                    validate_project_target(root, destination, &metadata)?;
                }
                let temp_metadata = fs::symlink_metadata(&temp.path)
                    .map_err(|error| config_error("inspect", &temp.path, error))?;
                if temp_metadata.file_type().is_symlink()
                    || !temp_metadata.is_file()
                    || !metadata_same_file(&written_metadata, &temp_metadata)
                {
                    return Err(Error::Config(
                        "temporary project config changed during write".to_string(),
                    ));
                }
                fs::rename(&temp.path, destination)
                    .map_err(|error| config_error("replace", destination, error))?;
                temp.armed = false;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(config_error("create", &temp_path, error)),
        }
    }
    Err(config_error(
        "create temporary config",
        config_dir,
        last_error.unwrap_or_else(|| std::io::Error::other("name collision")),
    ))
}

// The directory is born private. Creating it world-readable and narrowing it
// afterwards would expose a window in which another process can enter it or
// have the later chmod redirected onto a directory it swapped in.
#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|error| config_error("mkdir", path, error))
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir(path).map_err(|error| config_error("mkdir", path, error))
}

/// Walk up from `start` to the enclosing repo root — the first ancestor with a
/// `.git` or `.rtrt` entry. Returns `None` when `start` is not inside a repo,
/// so callers fall back to the plain global config.
pub fn repo_root_from(start: &Path) -> Option<PathBuf> {
    repo_root_in(std::iter::successors(Some(start), |dir| dir.parent()))
}

/// Core walk, parameterised over the ancestor sequence to examine.
///
/// Production always calls this with the *full*, unbounded ancestor chain of
/// `start` (a real `.git`/`.rtrt` anywhere above `start` legitimately wins).
/// Tests call it with a bounded, fixture-scoped ancestor list so their
/// assertions don't depend on what markers happen to exist above the system
/// temp dir on the machine running them.
fn repo_root_in<'a>(ancestors: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    for dir in ancestors {
        if dir.join(".git").exists() || dir.join(".rtrt").exists() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// User overrides for the invocation failure policy (`rtrt-providers`
/// `invoke_with_policy`): which error messages count as fatal / quota /
/// transient, and how a transient failure is retried.
///
/// The marker tables shipped in `rtrt-providers` remain the source of truth;
/// this section is purely additive. An absent `[failover]` section classifies
/// exactly like the built-ins, so the default behaviour is unchanged.
///
/// Precedence, highest first:
///   1. user `fatal`, then user `quota`, then user `transient`;
///   2. built-in fatal, then built-in quota, then built-in transient
///      (including the 5xx heuristic);
///   3. anything still unmatched is fatal.
///
/// Because the user layer is consulted first, listing a built-in marker under a
/// different class *reclassifies* it — e.g. putting `"timed out"` under `quota`
/// stops timeouts from earning a same-target retry.
///
/// Example `~/.rtrt/config.toml`:
///
/// ```toml
/// [failover]
/// quota = ["seat limit reached"]
/// fatal = ["contract expired"]
/// transient_retries = 1
/// backoff_divisor = 60
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverConfig {
    /// Extra markers that halt the walk: no retry, no failover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fatal: Vec<String>,
    /// Extra markers that fall over immediately, without retrying the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<String>,
    /// Extra markers that earn a backed-off retry on the same target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transient: Vec<String>,
    /// Same-target retries granted to a transient failure; `None` keeps the
    /// built-in single retry, `0` disables retrying entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient_retries: Option<u32>,
    /// Divisor applied to the per-call timeout to derive the retry backoff;
    /// `None` keeps the built-in divisor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_divisor: Option<u32>,
    /// Fixed retry backoff in milliseconds. Set only to pin the backoff; it
    /// overrides the timeout-derived value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<u64>,
}

impl FailoverConfig {
    /// True when nothing is customised, i.e. the policy is exactly the
    /// built-in one. Used to keep an untouched `[failover]` section out of the
    /// serialized config.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
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
            provider = "ollama"
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
        assert_eq!(c.auto_compress.provider.as_deref(), Some("ollama"));
        assert_eq!(
            c.auto_compress
                .effective_provider("https://example.test/v1"),
            "ollama"
        );
        // unset field keeps its default
        assert_eq!(c.auto_compress.age_sec, 3600);
        // unrelated section still defaults
        assert!(c.capture.enabled);
    }

    #[test]
    fn compatible_provider_identity_is_only_inferred_for_default_ollama_url() {
        let config = AutoCompressConfig::default();
        for (url, expected) in [
            ("http://127.0.0.1:11434/v1", "ollama"),
            ("http://localhost:11434", "ollama"),
            ("http://[::1]:11434/v1/", "ollama"),
            ("http://192.168.1.2:11434/v1", "openai-compat"),
            ("http://127.0.0.1:8080/v1", "openai-compat"),
            ("https://azure.example/openai/v1", "openai-compat"),
        ] {
            assert_eq!(config.effective_provider(url), expected, "{url}");
        }
    }

    #[test]
    fn provider_names_normalize_case_and_explicit_aliases() {
        for (input, expected) in [
            (" OpenAI ", "openai"),
            ("OPENAI-COMPATIBLE", "openai-compat"),
            ("openai_compatible", "openai-compat"),
            ("LMStudio", "lm-studio"),
            ("lms", "lm-studio"),
            ("llama_cpp", "llama.cpp"),
            ("llama", "llama.cpp"),
            ("vLLM", "vllm"),
            ("Azure-West", "azure-west"),
        ] {
            assert_eq!(normalize_provider_id(input), expected, "{input}");
        }

        let mut providers = ProvidersConfig::default();
        providers.enabled.insert("OpenAI".to_string(), false);
        assert_eq!(providers.enabled_override("openai"), Some(false));
    }

    #[test]
    fn limits_targets_are_case_insensitive_without_becoming_provider_identity() {
        let mut limits = LimitsConfig::default();
        limits.targets.insert(
            "OpenCode".to_string(),
            TargetLimit {
                daily_tokens: Some(10),
                ..TargetLimit::default()
            },
        );
        assert_eq!(
            limits
                .target("opencode")
                .and_then(|limit| limit.daily_tokens),
            Some(10)
        );
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
        // Pool caps are opt-in: a legacy target table declares none.
        assert!(openai.pools.is_empty());
        assert!(!openai.has_pool_limits());
        assert_eq!(c.limits.pool("openai", "anything"), None);
    }

    #[test]
    fn legacy_limits_toml_round_trips_without_pool_tables() {
        let legacy = r#"
            [limits.openai]
            daily_tokens = 1000000
            daily_requests = 2000

            [limits.ollama]
            daily_tokens = 250000
        "#;
        let config = Config::from_toml_str(legacy).unwrap();
        let serialized = toml::to_string(&config.limits).unwrap();
        // The new `pools` field must not appear for a config that never set it,
        // otherwise every existing ~/.rtrt/config.toml would be rewritten.
        assert!(
            !serialized.contains("pools"),
            "empty pools must be skipped on serialize:\n{serialized}"
        );
        let reparsed: LimitsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.target("openai").unwrap().daily_tokens,
            Some(1_000_000)
        );
        assert_eq!(
            reparsed.target("openai").unwrap().daily_requests,
            Some(2_000)
        );
        assert_eq!(
            reparsed.target("ollama").unwrap().daily_tokens,
            Some(250_000)
        );
        assert_eq!(reparsed.target("ollama").unwrap().daily_requests, None);
        assert_eq!(toml::to_string(&reparsed).unwrap(), serialized);
    }

    #[test]
    fn pool_limits_nest_under_their_target() {
        let c = Config::from_toml_str(
            r#"
            [limits.opencode]
            daily_tokens = 2_000_000

            [limits.opencode.pools.opencode-go]
            daily_tokens = 1_200_000

            [limits.opencode.pools.ollama]
            daily_requests = 500
            "#,
        )
        .unwrap();

        let opencode = c.limits.target("opencode").unwrap();
        // The target-wide cap keeps working exactly as before.
        assert_eq!(opencode.daily_tokens, Some(2_000_000));
        assert!(opencode.has_pool_limits());

        let go = c.limits.pool("opencode", "opencode-go").unwrap();
        assert_eq!(go.daily_tokens, Some(1_200_000));
        assert_eq!(go.daily_requests, None);
        let ollama = c.limits.pool("opencode", "ollama").unwrap();
        assert_eq!(ollama.daily_tokens, None);
        assert_eq!(ollama.daily_requests, Some(500));
        // An unconfigured pool has no cap — never a slice of the target's.
        assert!(c.limits.pool("opencode", "unknown-pool").is_none());
        assert!(c.limits.pool("claude", "opencode-go").is_none());
    }

    #[test]
    fn pool_lookup_is_case_insensitive() {
        let c = Config::from_toml_str(
            r#"
            [limits.opencode.pools.OpenCode-Go]
            daily_tokens = 10
            "#,
        )
        .unwrap();
        // Pool names derived from model strings are lowercased, so a config key
        // written with capitals must still match.
        assert_eq!(
            c.limits
                .pool("opencode", "opencode-go")
                .unwrap()
                .daily_tokens,
            Some(10)
        );
    }

    #[test]
    fn pool_limits_round_trip_through_toml() {
        let source = r#"
            [limits.opencode]
            daily_requests = 100

            [limits.opencode.pools.ollama]
            daily_tokens = 42
        "#;
        let config = Config::from_toml_str(source).unwrap();
        let serialized = toml::to_string(&config.limits).unwrap();
        let reparsed: LimitsConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.pool("opencode", "ollama").unwrap().daily_tokens,
            Some(42)
        );
        assert_eq!(
            reparsed.target("opencode").unwrap().daily_requests,
            Some(100)
        );
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

    // -----------------------------------------------------------------------
    // Per-project orchestration overrides (`[team]` / `[failover]` in
    // `<repo>/.rtrt/config.toml`). These assert the LAYERING CONTRACT — whole
    // section replacement, validated before it can become effective — without
    // pinning any shipped lane, tier or model name.
    // -----------------------------------------------------------------------

    /// A global config whose roster shares no lane name with the project one
    /// below, so "replaced" and "merged" cannot be confused.
    /// A minimal usable lane: identity plus the one role the validator insists
    /// every lane declares.
    #[test]
    fn project_and_global_config_reads_reject_oversize_before_parsing() {
        let repo = scratch_dir("rtrt-core-project-oversize");
        let config_dir = repo.join(".rtrt");
        std::fs::create_dir(&config_dir).unwrap();
        let project_path = config_dir.join("config.toml");
        let project_file = std::fs::File::create(&project_path).unwrap();
        project_file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        let error = Config::load_project(&repo).unwrap_err().to_string();
        assert!(error.contains("exceeds 1048576 bytes"), "{error}");

        let global_path = repo.join("global.toml");
        let global_file = std::fs::File::create(&global_path).unwrap();
        global_file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        let error = read_bounded_config(&global_path, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds 1048576 bytes"), "{error}");
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn oversized_save_preserves_existing_project_config_atomically() {
        let repo = scratch_dir("rtrt-core-project-atomic");
        let initial = ProjectConfig {
            output_level: Some("lite".to_string()),
            ..ProjectConfig::default()
        };
        Config::save_project(&repo, &initial).unwrap();
        let path = Config::project_config_path(&repo);
        let before = std::fs::read(&path).unwrap();

        let oversized = ProjectConfig {
            output_level: Some("x".repeat(MAX_CONFIG_BYTES as usize)),
            ..ProjectConfig::default()
        };
        let error = Config::save_project(&repo, &oversized)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds 1048576 bytes"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(repo.join(".rtrt")).unwrap().count(),
            1,
            "failed save left a sibling temporary file"
        );
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn project_config_rejects_symlink_traversal_and_symlink_targets() {
        use std::os::unix::fs::symlink;

        let repo = scratch_dir("rtrt-core-project-symlink");
        let outside = scratch_dir("rtrt-core-project-outside");
        symlink(&outside, repo.join(".rtrt")).unwrap();
        let over = ProjectConfig {
            output_level: Some("full".to_string()),
            ..ProjectConfig::default()
        };
        assert!(Config::save_project(&repo, &over).is_err());
        assert!(Config::load_project(&repo).is_err());
        assert!(!outside.join("config.toml").exists());
        std::fs::remove_file(repo.join(".rtrt")).unwrap();

        std::fs::create_dir(repo.join(".rtrt")).unwrap();
        let foreign = outside.join("foreign.toml");
        std::fs::write(&foreign, "output_level = \"lite\"\n").unwrap();
        let path = Config::project_config_path(&repo);
        symlink(&foreign, &path).unwrap();
        assert!(Config::load_project(&repo).is_err());
        assert!(Config::save_project(&repo, &over).is_err());
        assert!(Config::save_project(&repo, &ProjectConfig::default()).is_err());
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "output_level = \"lite\"\n"
        );
        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn project_config_rejects_non_directory_components_and_non_regular_targets() {
        let repo = scratch_dir("rtrt-core-project-components");
        std::fs::write(repo.join(".rtrt"), b"not a directory").unwrap();
        assert!(Config::load_project(&repo).is_err());
        assert!(Config::save_project(&repo, &ProjectConfig::default()).is_err());
        std::fs::remove_file(repo.join(".rtrt")).unwrap();

        std::fs::create_dir(repo.join(".rtrt")).unwrap();
        std::fs::create_dir(Config::project_config_path(&repo)).unwrap();
        assert!(Config::load_project(&repo).is_err());
        assert!(Config::save_project(&repo, &ProjectConfig::default()).is_err());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    /// A unique scratch directory for the file-touching tests above.
    fn scratch_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

        // `repo_root_from` walks all the way to the filesystem root, so a
        // stray `.git`/`.rtrt` above the system temp dir (this machine has
        // one at `/tmp/.git`) would legitimately win and make the "no
        // marker yet" assertion depend on the environment. Bound the
        // ancestor walk to the fixture itself so the test only ever
        // inspects directories it created.
        let bounded_ancestors = || {
            let v: Vec<&Path> = std::iter::successors(Some(nested.as_path()), |d| d.parent())
                .take_while(|d| d.starts_with(&root))
                .collect();
            v
        };
        assert_eq!(repo_root_in(bounded_ancestors().into_iter()), None);
        std::fs::create_dir_all(root.join(".rtrt")).unwrap();
        assert_eq!(
            repo_root_in(bounded_ancestors().into_iter()),
            Some(root.clone())
        );

        // The unbounded production entry point still finds the marker we
        // just created (real markers above `root`, if any, are shadowed by
        // it since it's closer).
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
