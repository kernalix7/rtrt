use async_trait::async_trait;
use rtrt_core::{Error, Result};

use crate::{ChatRequest, ChatResponse, Provider};

/// OpenAI-compatible HTTP endpoint adapter — Ollama, llama.cpp server, vLLM, LM Studio, etc.
pub struct OpenAICompatibleProvider {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub http: reqwest::Client,
}

impl OpenAICompatibleProvider {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            api_key: None,
            http: reqwest::Client::new(),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
}

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported_models(&self) -> &[&'static str] {
        &[]
    }

    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Err(Error::Provider(format!(
            "{}: openai-compatible chat not implemented yet",
            self.name
        )))
    }
}
