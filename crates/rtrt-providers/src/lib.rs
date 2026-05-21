//! rtrt-providers — provider abstraction for chat / completion APIs.
//!
//! Built-in adapters: Anthropic, OpenAI, and any OpenAI-compatible local server
//! (Ollama, llama.cpp, vLLM, LM Studio). External adapters load as plugins.

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use rtrt_core::{Error, Result};
use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod context7;
pub mod gateway;
pub mod openai;
pub mod openai_compatible;
pub mod stream;
pub mod usage;

pub use anthropic::AnthropicProvider;
pub use context7::Context7Client;
pub use gateway::{
    Budget, Gateway, GatewaySummary, MetricsView, ModelPricing, RequestMetric, default_pricing,
};
pub use openai::OpenAIProvider;
pub use openai_compatible::OpenAICompatibleProvider;
pub use usage::Usage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub provider: String,
    pub model: String,
    pub content: String,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatStreamEvent {
    Delta { text: String },
    Usage(Usage),
    Done,
}

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent>> + Send>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn supported_models(&self) -> &[&'static str];
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
    async fn chat_stream(&self, _req: ChatRequest) -> Result<ChatStream> {
        Err(Error::Provider(format!(
            "{}: streaming not implemented",
            self.name()
        )))
    }
}
