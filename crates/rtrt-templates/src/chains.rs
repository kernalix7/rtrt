//! `llm-chain`-style sequential prompt composition.
//!
//! A [`PromptChain`] is an ordered list of [`PromptStep`]s. Each step has a
//! system prompt and a user-prompt template. The user template is rendered
//! with handlebars; the previous step's output is available as `{{previous}}`,
//! and arbitrary caller-supplied variables flow through unchanged.
//!
//! Gated behind the `chains` feature so the base `rtrt-templates` crate
//! doesn't pull `reqwest` into the dep tree.

use std::collections::BTreeMap;

use handlebars::Handlebars;
use rtrt_core::{Error, Result};
use rtrt_providers::{ChatMessage, ChatRequest, Provider, Role};

#[derive(Debug, Clone)]
pub struct PromptStep {
    pub name: String,
    pub system: String,
    pub user: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

impl PromptStep {
    pub fn new(name: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            system: String::new(),
            user: user.into(),
            max_tokens: None,
            temperature: None,
        }
    }

    pub fn with_system(mut self, sys: impl Into<String>) -> Self {
        self.system = sys.into();
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = Some(n);
        self
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t);
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptChain {
    steps: Vec<PromptStep>,
}

impl PromptChain {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn step(mut self, step: PromptStep) -> Self {
        self.steps.push(step);
        self
    }

    /// Runs each step in order. The previous step's response body is bound to
    /// `{{previous}}` in the next step's user-prompt template. `vars` flows
    /// into every step. Returns the full list of step outputs (same length
    /// as the chain).
    pub async fn execute(
        &self,
        provider: &dyn Provider,
        model: &str,
        vars: &BTreeMap<String, String>,
    ) -> Result<Vec<StepOutput>> {
        let hbs = Handlebars::new();
        let mut outputs = Vec::with_capacity(self.steps.len());
        let mut previous = String::new();
        for step in &self.steps {
            let mut context = vars.clone();
            context.insert("previous".into(), previous.clone());
            let rendered = hbs
                .render_template(&step.user, &context)
                .map_err(|e| Error::Config(format!("chain '{}': {e}", step.name)))?;
            let mut messages = Vec::new();
            if !step.system.is_empty() {
                messages.push(ChatMessage {
                    role: Role::System,
                    content: step.system.clone(),
                });
            }
            messages.push(ChatMessage {
                role: Role::User,
                content: rendered,
            });
            let req = ChatRequest {
                model: model.to_string(),
                messages,
                max_tokens: step.max_tokens,
                temperature: step.temperature,
            };
            let resp = provider.chat(req).await?;
            previous = resp.content.clone();
            outputs.push(StepOutput {
                name: step.name.clone(),
                content: resp.content,
                provider: resp.provider,
                model: resp.model,
                input_tokens: resp.usage.input_tokens,
                output_tokens: resp.usage.output_tokens,
            });
        }
        Ok(outputs)
    }
}

#[derive(Debug, Clone)]
pub struct StepOutput {
    pub name: String,
    pub content: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rtrt_providers::{ChatResponse, Usage};

    struct EchoUpper;

    #[async_trait]
    impl Provider for EchoUpper {
        fn name(&self) -> &str {
            "echo-upper"
        }
        fn supported_models(&self) -> &[&'static str] {
            &[]
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
            let user = req
                .messages
                .iter()
                .find(|m| matches!(m.role, Role::User))
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(ChatResponse {
                provider: "echo-upper".to_string(),
                model: req.model,
                content: user.to_uppercase(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
            })
        }
    }

    #[tokio::test]
    async fn chain_threads_previous_into_next() {
        let chain = PromptChain::new()
            .step(PromptStep::new("title", "title: {{topic}}"))
            .step(PromptStep::new("expand", "expand: {{previous}}"));
        let mut vars = BTreeMap::new();
        vars.insert("topic".into(), "hello".into());
        let out = chain.execute(&EchoUpper, "x", &vars).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "TITLE: HELLO");
        // Second step receives the first step's uppercased output as `{{previous}}`.
        assert_eq!(out[1].content, "EXPAND: TITLE: HELLO");
        assert_eq!(out[1].name, "expand");
    }
}
