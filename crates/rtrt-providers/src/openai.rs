//! OpenAI Chat Completions API adapter — POST /v1/chat/completions.
//!
//! Handles both unary and SSE streaming. Usage on streaming is provided when the
//! caller sets `stream_options.include_usage = true` on the request payload —
//! we always set it so the final chunk carries the usage block.
//!
//! Every response also carries `x-ratelimit-*` headers stating the quota rtrt
//! has left. They are captured into the rate-limit signal store on both the
//! success and the error path — see [`OpenAIProvider::capture_rate_limit`].

use async_trait::async_trait;
use rtrt_core::{Error, PoolKey, Result};
use serde::Deserialize;
use serde_json::json;

use crate::{
    ChatRequest, ChatResponse, ChatStream, ChatStreamEvent, Provider, Role, Usage, stream,
    usage_ledger,
};

/// The quota bucket OpenAI API responses are filed under by default.
pub const OPENAI_TARGET: &str = "openai";

pub struct OpenAIProvider {
    pub api_key: String,
    pub base_url: String,
    pub http: reqwest::Client,
    /// The quota bucket this endpoint's rate-limit headers are recorded under.
    ///
    /// Defaults to [`OPENAI_TARGET`]. [`crate::OpenAICompatibleProvider`]
    /// overrides it with its own name, so a local llama.cpp server's headers are
    /// never filed against OpenAI's quota.
    pub usage_target: String,
}

impl OpenAIProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
            http: reqwest::Client::new(),
            usage_target: OPENAI_TARGET.to_string(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Record this endpoint's rate-limit signals under a different quota bucket
    /// than `openai` — what an OpenAI-compatible server needs.
    pub fn with_usage_target(mut self, target: impl Into<String>) -> Self {
        self.usage_target = target.into();
        self
    }

    /// File this response's rate-limit headers under the pool the call drew
    /// from.
    ///
    /// Called on every response, success or failure: a 429 is exactly when the
    /// remaining / reset numbers matter, and it is the one response guaranteed
    /// to carry them. Best-effort by contract — the recorder never returns an
    /// error and never fails the request.
    fn capture_rate_limit(&self, model: &str, resp: &reqwest::Response) {
        usage_ledger::record_response_rate_limit(
            &PoolKey::from_target_model(&self.usage_target, Some(model)),
            resp.status().as_u16(),
            resp.headers(),
        );
    }

    fn build_payload(&self, req: &ChatRequest, stream: bool) -> serde_json::Value {
        let msgs: Vec<_> = req
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();
        let mut body = json!({
            "model": req.model,
            "messages": msgs,
            "stream": stream,
        });
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = json!(max);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    fn name(&self) -> &str {
        OPENAI_TARGET
    }

    fn supported_models(&self) -> &[&'static str] {
        &["gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark"]
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_payload(&req, false);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("openai request: {e}")))?;
        let status = resp.status();
        self.capture_rate_limit(&req.model, &resp);
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("openai {status}: {body}")));
        }
        let parsed: ChatCompletion = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("openai decode: {e}")))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        Ok(ChatResponse {
            provider: self.name().to_string(),
            model: parsed.model.unwrap_or(req.model),
            content,
            usage: parsed.usage.unwrap_or_default().into(),
        })
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_payload(&req, true);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("openai stream request: {e}")))?;
        let status = resp.status();
        self.capture_rate_limit(&req.model, &resp);
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("openai {status}: {body}")));
        }
        Ok(stream::decode(resp, decode_event))
    }
}

fn decode_event(_event: &str, data: &str) -> Result<Option<ChatStreamEvent>> {
    if data == "[DONE]" {
        return Ok(Some(ChatStreamEvent::Done));
    }
    let v: StreamChunk = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    if let Some(u) = v.usage {
        return Ok(Some(ChatStreamEvent::Usage(u.into())));
    }
    if let Some(choice) = v.choices.into_iter().next() {
        if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                return Ok(Some(ChatStreamEvent::Delta { text }));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Message,
}

#[derive(Debug, Default, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl From<OpenAIUsage> for Usage {
    fn from(u: OpenAIUsage) -> Self {
        Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_read_input_tokens: u
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
            cache_creation_input_tokens: 0,
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, ChatRequest, Role};

    #[tokio::test]
    async fn chat_round_trip() {
        if loopback_bind_unavailable() {
            return;
        }
        let mut server = mockito::Server::new_async().await;
        let body = serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-5.4-mini",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "pong" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 1 }
        });
        let _m = server
            .mock("POST", "/chat/completions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create_async()
            .await;
        let provider = OpenAIProvider::new("test").with_base_url(server.url());
        let resp = provider
            .chat(ChatRequest {
                model: "gpt-5.4-mini".into(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: "ping".into(),
                }],
                max_tokens: None,
                temperature: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.content, "pong");
        assert_eq!(resp.usage.input_tokens, 5);
        assert_eq!(resp.usage.output_tokens, 1);
    }

    fn loopback_bind_unavailable() -> bool {
        std::net::TcpListener::bind("127.0.0.1:0").is_err()
    }
}
