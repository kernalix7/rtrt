//! OpenAI-compatible HTTP endpoint adapter — Ollama, llama.cpp server, vLLM, LM Studio, etc.
//!
//! Re-uses [`OpenAIProvider`]'s wire format. The differences vs `OpenAIProvider`:
//! - `name` is user-provided so dashboards can distinguish (e.g. `"ollama"`, `"vllm"`).
//! - `api_key` is optional — many local servers don't require auth.

use async_trait::async_trait;
use rtrt_core::Result;

use crate::{ChatRequest, ChatResponse, ChatStream, OpenAIProvider, Provider};

pub struct OpenAICompatibleProvider {
    pub name: String,
    inner: OpenAIProvider,
}

impl OpenAICompatibleProvider {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            inner: OpenAIProvider::new(String::new()).with_base_url(base_url),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.inner.api_key = key.into();
        self
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.inner = self.inner.with_http(http);
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

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut resp = self.inner.chat(req).await?;
        resp.provider = self.name.clone();
        Ok(resp)
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        self.inner.chat_stream(req).await
    }
}
