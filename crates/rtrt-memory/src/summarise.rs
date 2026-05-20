//! LLM-backed memory extraction and compression.
//!
//! The [`Summariser`] trait is always available so call sites can be written
//! once. The default [`LlmSummariser`] implementation lives behind the `llm`
//! feature and uses any [`rtrt_providers::Provider`] — Anthropic, OpenAI, or
//! an OpenAI-compatible local endpoint (Ollama, llama.cpp server, vLLM,
//! LM Studio). Cloud and local backends share the same wire format.
//!
//! ## Operations
//!
//! - **extract** — turns a long passage into a list of atomic, recall-friendly
//!   facts. Use to ingest a wiki page or chat transcript without storing
//!   pre-chewed prose.
//! - **summarise** — collapses a batch of older memories into a single
//!   archival entry. Use to keep the working memory pool small.

use async_trait::async_trait;
use rtrt_core::Result;

#[async_trait]
pub trait Summariser: Send + Sync {
    /// Returns the model id the summariser will route to.
    fn model(&self) -> &str;

    /// Collapses `text` into a single short paragraph. Use for memory compression.
    async fn summarise(&self, text: &str) -> Result<String>;

    /// Splits `text` into atomic facts, one per item. Use for ingestion.
    async fn extract_atomic(&self, text: &str) -> Result<Vec<String>>;
}

#[cfg(feature = "llm")]
mod llm_impl {
    use async_trait::async_trait;
    use rtrt_core::{Error, Result};
    use rtrt_providers::{ChatMessage, ChatRequest, Provider, Role};

    use super::Summariser;

    /// LLM-backed summariser. Wraps any [`Provider`] (cloud or local).
    pub struct LlmSummariser {
        provider: Box<dyn Provider>,
        model: String,
        max_tokens: u32,
    }

    impl LlmSummariser {
        pub fn new(provider: Box<dyn Provider>, model: impl Into<String>) -> Self {
            Self { provider, model: model.into(), max_tokens: 512 }
        }

        pub fn with_max_tokens(mut self, n: u32) -> Self {
            self.max_tokens = n;
            self
        }

        async fn chat(&self, system: &str, user: &str) -> Result<String> {
            let req = ChatRequest {
                model: self.model.clone(),
                messages: vec![
                    ChatMessage { role: Role::System, content: system.to_string() },
                    ChatMessage { role: Role::User, content: user.to_string() },
                ],
                max_tokens: Some(self.max_tokens),
                temperature: Some(0.2),
            };
            let resp = self.provider.chat(req).await?;
            Ok(resp.content)
        }
    }

    const SUMMARISE_SYS: &str = "You compress agent memory. Given a passage, write one concise factual paragraph that preserves names, dates, numbers, and decisions. No filler, no apologies, no markdown.";

    const EXTRACT_SYS: &str = "You extract atomic memory facts. Read the passage and emit one fact per line. Each fact is a single short sentence covering exactly one piece of information (a name, a date, a number, a relationship, a decision). Output only the lines, no numbering, no bullets, no commentary.";

    #[async_trait]
    impl Summariser for LlmSummariser {
        fn model(&self) -> &str {
            &self.model
        }

        async fn summarise(&self, text: &str) -> Result<String> {
            let out = self.chat(SUMMARISE_SYS, text).await?;
            let trimmed = out.trim();
            if trimmed.is_empty() {
                return Err(Error::Memory("summariser returned empty text".into()));
            }
            Ok(trimmed.to_string())
        }

        async fn extract_atomic(&self, text: &str) -> Result<Vec<String>> {
            let out = self.chat(EXTRACT_SYS, text).await?;
            let lines: Vec<String> = out
                .lines()
                .map(|l| l.trim().trim_start_matches(['-', '*', '•', '·']).trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if lines.is_empty() {
                return Err(Error::Memory("extract_atomic returned no lines".into()));
            }
            Ok(lines)
        }
    }
}

#[cfg(feature = "llm")]
pub use llm_impl::LlmSummariser;

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Mock summariser used by [`crate::tests`] to exercise extract/compress
    /// without spinning up a real LLM.
    pub(crate) struct MockSummariser;

    #[async_trait]
    impl Summariser for MockSummariser {
        fn model(&self) -> &str {
            "mock"
        }
        async fn summarise(&self, text: &str) -> Result<String> {
            Ok(format!("[summary] {}", text.chars().take(40).collect::<String>()))
        }
        async fn extract_atomic(&self, text: &str) -> Result<Vec<String>> {
            Ok(text.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarise::test_support::MockSummariser;

    #[tokio::test]
    async fn mock_extracts_separated_facts() {
        let s = MockSummariser;
        let out = s.extract_atomic("a is 1; b is 2; c is 3").await.unwrap();
        assert_eq!(out, vec!["a is 1", "b is 2", "c is 3"]);
    }
}
