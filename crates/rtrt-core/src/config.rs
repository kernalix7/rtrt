use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CompressionLevel, Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            compression: CompressionConfig::default(),
            memory: MemoryConfig::default(),
            dashboard: DashboardConfig::default(),
            providers: ProvidersConfig::default(),
        }
    }
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
        Self { level: CompressionLevel::default(), enabled: true }
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
        Self { path: default_memory_path(), embed_model: default_embed_model() }
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
        Self { bind: default_dashboard_addr() }
    }
}

fn default_dashboard_addr() -> String {
    "127.0.0.1:3111".to_string()
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
        toml_lite::from_str(s).map_err(|e| Error::Config(e.to_string()))
    }
}

mod toml_lite {
    //! Tiny JSON-over-TOML adapter so we don't pull a full TOML crate in scaffold.
    //! Replaced by `toml` crate once parsing is actually needed.
    use serde::de::DeserializeOwned;

    pub fn from_str<T: DeserializeOwned>(s: &str) -> Result<T, String> {
        let _ = s;
        Err("TOML config parsing not wired yet; use defaults via Config::default()".into())
    }
}
