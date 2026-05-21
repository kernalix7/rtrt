//! Anthropic Messages API adapter — POST /v1/messages.
//!
//! Maps RTRT's flat [`ChatMessage`] list to Anthropic's `system` field + `messages`
//! array. Handles both unary and streaming responses. Usage is parsed from the
//! `usage` field on unary, and from `message_start` / `message_delta` events on
//! streaming.

use async_trait::async_trait;
use rtrt_core::{Error, Result};
use serde::Deserialize;
use serde_json::json;

use crate::{
    ChatRequest, ChatResponse, ChatStream, ChatStreamEvent, Provider, Role, Usage, stream,
};

pub struct AnthropicProvider {
    pub api_key: String,
    pub base_url: String,
    pub http: reqwest::Client,
    /// When the joined system prompt is at least this many characters, the
    /// adapter attaches `cache_control: { type: "ephemeral" }` so Anthropic
    /// reuses the cached prefix across requests. `0` disables the heuristic.
    pub cache_threshold: usize,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            http: reqwest::Client::new(),
            // 1024 chars is well above Anthropic's 1024-token minimum for
            // prompt caching once compression is applied; raise this if the
            // workload always sends short system prompts.
            cache_threshold: 1024,
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

    /// Override the system-prompt length at which `cache_control: ephemeral`
    /// is attached. Set to `0` to disable prompt caching.
    pub fn with_cache_threshold(mut self, chars: usize) -> Self {
        self.cache_threshold = chars;
        self
    }

    fn build_payload(&self, req: &ChatRequest, stream: bool) -> serde_json::Value {
        let (system, msgs) = split_system(&req.messages);
        let mut body = json!({
            "model": req.model,
            "messages": msgs,
            "max_tokens": req.max_tokens.unwrap_or(1024),
            "stream": stream,
        });
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        if !system.is_empty() {
            // Anthropic's prompt cache wants the system field as an array of
            // content blocks with an optional `cache_control` marker. Long
            // system prompts (compressed agent context, multi-page rules) are
            // the typical caching target, so we gate on a configurable length.
            if self.cache_threshold > 0 && system.chars().count() >= self.cache_threshold {
                body["system"] = json!([
                    {
                        "type": "text",
                        "text": system,
                        "cache_control": { "type": "ephemeral" },
                    }
                ]);
            } else {
                body["system"] = json!(system);
            }
        }
        body
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn supported_models(&self) -> &[&'static str] {
        &["claude-opus-4-7", "claude-sonnet-4-6", "claude-haiku-4-5"]
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/messages", self.base_url);
        let body = self.build_payload(&req, false);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("anthropic request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("anthropic {status}: {body}")));
        }
        let parsed: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| Error::Provider(format!("anthropic decode: {e}")))?;
        let content = parsed
            .content
            .into_iter()
            .filter_map(|b| {
                if b.r#type == "text" {
                    Some(b.text.unwrap_or_default())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(ChatResponse {
            provider: self.name().to_string(),
            model: parsed.model.unwrap_or(req.model),
            content,
            usage: parsed.usage.unwrap_or_default(),
        })
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<ChatStream> {
        let url = format!("{}/messages", self.base_url);
        let body = self.build_payload(&req, true);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("anthropic stream request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("anthropic {status}: {body}")));
        }
        Ok(stream::decode(resp, decode_event))
    }
}

fn decode_event(event: &str, data: &str) -> Result<Option<ChatStreamEvent>> {
    if data == "[DONE]" {
        return Ok(Some(ChatStreamEvent::Done));
    }
    match event {
        "content_block_delta" => {
            let v: ContentBlockDelta = serde_json::from_str(data)
                .map_err(|e| Error::Provider(format!("anthropic delta: {e}")))?;
            if v.delta.r#type == "text_delta" {
                if let Some(text) = v.delta.text {
                    return Ok(Some(ChatStreamEvent::Delta { text }));
                }
            }
            Ok(None)
        }
        "message_start" => {
            let v: MessageStart = serde_json::from_str(data)
                .map_err(|e| Error::Provider(format!("anthropic message_start: {e}")))?;
            Ok(Some(ChatStreamEvent::Usage(
                v.message.usage.unwrap_or_default(),
            )))
        }
        "message_delta" => {
            let v: MessageDelta = serde_json::from_str(data)
                .map_err(|e| Error::Provider(format!("anthropic message_delta: {e}")))?;
            if let Some(u) = v.usage {
                return Ok(Some(ChatStreamEvent::Usage(u)));
            }
            Ok(None)
        }
        "message_stop" => Ok(Some(ChatStreamEvent::Done)),
        _ => Ok(None),
    }
}

fn split_system(messages: &[crate::ChatMessage]) -> (String, Vec<serde_json::Value>) {
    let mut system_parts = Vec::new();
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system_parts.push(m.content.clone()),
            Role::User => out.push(json!({ "role": "user", "content": m.content })),
            Role::Assistant => out.push(json!({ "role": "assistant", "content": m.content })),
        }
    }
    (system_parts.join("\n\n"), out)
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDelta {
    delta: ContentBlockDeltaInner,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDeltaInner {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    message: MessageStartInner,
}

#[derive(Debug, Deserialize)]
struct MessageStartInner {
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct MessageDelta {
    #[serde(default)]
    usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, ChatRequest, Role};

    #[tokio::test]
    async fn chat_round_trip() {
        let mut server = mockito::Server::new_async().await;
        let body = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "model": "claude-haiku-4-5",
            "content": [{ "type": "text", "text": "hi!" }],
            "usage": { "input_tokens": 12, "output_tokens": 3 }
        });
        let _m = server
            .mock("POST", "/messages")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body.to_string())
            .create_async()
            .await;
        let provider = AnthropicProvider::new("test").with_base_url(server.url());
        let resp = provider
            .chat(ChatRequest {
                model: "claude-haiku-4-5".into(),
                messages: vec![
                    ChatMessage {
                        role: Role::System,
                        content: "be terse".into(),
                    },
                    ChatMessage {
                        role: Role::User,
                        content: "hi".into(),
                    },
                ],
                max_tokens: Some(50),
                temperature: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.content, "hi!");
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 3);
    }

    #[test]
    fn split_system_groups() {
        let msgs = vec![
            ChatMessage {
                role: Role::System,
                content: "S1".into(),
            },
            ChatMessage {
                role: Role::System,
                content: "S2".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "U".into(),
            },
        ];
        let (sys, out) = split_system(&msgs);
        assert_eq!(sys, "S1\n\nS2");
        assert_eq!(out.len(), 1);
    }
}
