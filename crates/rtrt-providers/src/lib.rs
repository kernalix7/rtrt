//! rtrt-providers — provider abstraction for chat / completion APIs.
//!
//! Built-in adapters: Anthropic, OpenAI, Google, xAI, Mistral, and any OpenAI-compatible
//! local server (Ollama, llama.cpp, vLLM, LM Studio). External adapters load as plugins.

use async_trait::async_trait;
use rtrt_core::Result;
use serde::{Deserialize, Serialize};

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
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn supported_models(&self) -> &[&'static str];
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
}

pub mod anthropic;
pub mod openai;
pub mod openai_compatible;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAIProvider;
pub use openai_compatible::OpenAICompatibleProvider;
