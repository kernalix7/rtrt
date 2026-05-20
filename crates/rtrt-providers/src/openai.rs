use async_trait::async_trait;
use rtrt_core::{Error, Result};

use crate::{ChatRequest, ChatResponse, Provider};

pub struct OpenAIProvider {
    pub api_key: String,
    pub base_url: String,
    pub http: reqwest::Client,
}

impl OpenAIProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn supported_models(&self) -> &[&'static str] {
        &["gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark"]
    }

    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Err(Error::Provider("openai chat not implemented yet".into()))
    }
}
