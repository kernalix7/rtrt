use async_trait::async_trait;
use rtrt_core::{Error, Result};

use crate::{ChatRequest, ChatResponse, Provider};

pub struct AnthropicProvider {
    pub api_key: String,
    pub base_url: String,
    pub http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn supported_models(&self) -> &[&'static str] {
        &[
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ]
    }

    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse> {
        Err(Error::Provider("anthropic chat not implemented yet".into()))
    }
}
