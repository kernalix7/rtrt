//! OpenAI-compatible HTTP endpoint adapter — Ollama, llama.cpp server, vLLM, LM Studio, etc.
//!
//! Re-uses [`OpenAIProvider`]'s wire format. The differences vs `OpenAIProvider`:
//! - `name` is user-provided so dashboards can distinguish (e.g. `"ollama"`, `"vllm"`).
//! - `api_key` is optional — many local servers don't require auth.
//! - rate-limit signals are recorded under that same user-provided name, so a
//!   local server that emits `x-ratelimit-*` headers gets its own quota bucket
//!   instead of drawing down OpenAI's.

use async_trait::async_trait;
use rtrt_core::{Result, config::normalize_provider_id};

use crate::{ChatRequest, ChatResponse, ChatStream, OpenAIProvider, Provider};

pub struct OpenAICompatibleProvider {
    provider_id: String,
    models: Vec<String>,
    inner: OpenAIProvider,
}

impl OpenAICompatibleProvider {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        let provider_id = normalize_provider_id(&name.into());
        Self {
            inner: OpenAIProvider::new(String::new())
                .with_base_url(base_url)
                .with_usage_target(&provider_id),
            provider_id,
            models: Vec::new(),
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

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        let model = model.trim();
        let model = model
            .split_once('/')
            .filter(|(prefix, _)| normalize_provider_id(prefix) == self.provider_id)
            .map_or(model, |(_, upstream)| upstream);
        if !model.is_empty() && !self.models.iter().any(|existing| existing == model) {
            self.models.push(model.to_string());
        }
        self
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub const fn transport_id(&self) -> &'static str {
        "openai-compatible"
    }
}

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn name(&self) -> &str {
        &self.provider_id
    }

    fn transport(&self) -> &str {
        self.transport_id()
    }

    fn supported_models(&self) -> &[&'static str] {
        &[]
    }

    fn model_ids(&self) -> Vec<String> {
        self.models.clone()
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let mut resp = self.inner.chat(req).await?;
        resp.provider = self.provider_id.clone();
        Ok(resp)
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        self.inner.chat_stream(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_identity_is_independent_of_transport() {
        let provider = OpenAICompatibleProvider::new("ollama", "http://127.0.0.1:11434/v1");
        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.provider_id(), "ollama");
        assert_eq!(provider.transport(), "openai-compatible");
    }

    #[test]
    fn provider_aliases_normalize_and_canonical_models_are_not_double_prefixed() {
        let provider = OpenAICompatibleProvider::new("OLLAMA", "http://127.0.0.1:11434/v1")
            .with_model("Ollama/org/model:tag");
        assert_eq!(provider.provider_id(), "ollama");
        assert_eq!(provider.model_ids(), vec!["org/model:tag"]);

        let neutral = OpenAICompatibleProvider::new("openai-compatible", "https://example.test");
        assert_eq!(neutral.provider_id(), "openai-compat");
        assert_ne!(neutral.provider_id(), neutral.transport());
    }
}
