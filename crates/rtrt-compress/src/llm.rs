//! LLM-backed compression.
//!
//! Routes the input through any [`rtrt_providers::Provider`] (Anthropic,
//! OpenAI, or a local Ollama / llama.cpp / vLLM / LM Studio endpoint exposing
//! the OpenAI-compatible API) and asks the model to rewrite the passage tersely
//! while preserving every piece of technical content.
//!
//! This is the path to Output Optimizer 50-75% savings on natural prose — those
//! numbers come from LLM-driven rewriting, not from regex deletion. Pair with
//! [`crate::Compressor`] only when you also want the rule-based stripping on
//! top of the LLM output.
//!
//! Gated behind the `llm-compress` feature so the base `rtrt-compress` crate
//! doesn't pull in `reqwest` for callers who only want the rule pass.

use async_trait::async_trait;
use rtrt_core::{Error, Result};
use rtrt_providers::{ChatMessage, ChatRequest, Provider, Role};

use crate::redact_secrets;

#[async_trait]
pub trait AsyncCompressor: Send + Sync {
    fn model(&self) -> &str;
    async fn compress(&self, text: &str) -> Result<String>;
}

pub struct LlmCompressor {
    provider: Box<dyn Provider>,
    model: String,
    max_tokens: u32,
    temperature: f32,
    system_prompt: String,
}

impl LlmCompressor {
    pub fn new(provider: Box<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            max_tokens: 2048,
            temperature: 0.1,
            system_prompt: default_system_prompt().to_string(),
        }
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = t;
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }
}

#[async_trait]
impl AsyncCompressor for LlmCompressor {
    fn model(&self) -> &str {
        &self.model
    }

    async fn compress(&self, text: &str) -> Result<String> {
        let redacted = redact_secrets(text);
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage {
                    role: Role::System,
                    content: self.system_prompt.clone(),
                },
                ChatMessage {
                    role: Role::User,
                    content: redacted,
                },
            ],
            max_tokens: Some(self.max_tokens),
            temperature: Some(self.temperature),
        };
        let resp = self.provider.chat(req).await?;
        let trimmed = resp.content.trim();
        if trimmed.is_empty() {
            return Err(Error::Provider("llm compressor returned empty text".into()));
        }
        Ok(trimmed.to_string())
    }
}

fn default_system_prompt() -> &'static str {
    "You compress agent output. Rewrite the passage in the fewest words that preserve every fact, name, number, date, identifier, code block, inline `code`, URL, quoted error string, file path, and command. Drop fillers (just, really, basically, actually, simply, literally), pleasantries (sure, certainly, of course, happy to), hedging (I think, perhaps, maybe), and discourse markers (moreover, however, as you can see). Use fragments. No apologies. No preamble. No restatement of the question. Output only the compressed passage, nothing else."
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rtrt_providers::{ChatRequest, ChatResponse, Provider, Usage};

    struct EchoProvider;

    #[async_trait]
    impl Provider for EchoProvider {
        fn name(&self) -> &str {
            "echo"
        }
        fn supported_models(&self) -> &[&'static str] {
            &["echo-1"]
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
            let last = req.messages.last().expect("user message");
            let content = format!(
                "compressed: {}",
                last.content
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            Ok(ChatResponse {
                provider: "echo".to_string(),
                model: req.model,
                content,
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    ..Default::default()
                },
            })
        }
    }

    #[tokio::test]
    async fn llm_compressor_routes_through_provider() {
        let c = LlmCompressor::new(Box::new(EchoProvider), "echo-1");
        let out = c.compress("hello there world how are you").await.unwrap();
        assert!(out.starts_with("compressed:"), "{out}");
    }

    #[tokio::test]
    async fn llm_compressor_redacts_secrets_first() {
        let c = LlmCompressor::new(Box::new(EchoProvider), "echo-1");
        let out = c
            .compress("token sk-ant-abcdef1234567890ghijkl rest")
            .await
            .unwrap();
        assert!(!out.contains("sk-ant-abcdef"), "{out}");
    }
}
