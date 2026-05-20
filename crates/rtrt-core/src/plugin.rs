use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Compressor,
    Proxy,
    Memory,
    Provider,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMetadata {
    pub name: String,
    pub version: String,
    pub kind: PluginKind,
    #[serde(default)]
    pub description: Option<String>,
}

#[async_trait]
pub trait Plugin: Send + Sync {
    fn metadata(&self) -> &PluginMetadata;

    async fn init(&mut self) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
