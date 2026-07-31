//! Provider adapters → the provider-reported rate-limit signal store.
//!
//! Each adapter must capture what the provider says about its own quota on the
//! success path *and* on the rejection path, file it under the pool the call
//! actually drew from, and never let a recording problem change what the caller
//! sees. A response that says nothing must leave nothing behind.
//!
//! One test function on purpose: the signal path is selected via the
//! process-global `RTRT_PROVIDER_RATELIMIT_PATH` env var, so splitting into
//! parallel tests would race on it.

use rtrt_providers::{
    AnthropicProvider, ChatMessage, ChatRequest, OpenAICompatibleProvider, OpenAIProvider,
    Provider, Role, rate_limit_signals,
};

fn req(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![ChatMessage {
            role: Role::User,
            content: "ping".into(),
        }],
        max_tokens: None,
        temperature: None,
    }
}

fn anthropic_body() -> String {
    serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "model": "claude-haiku-4-5",
        "content": [{ "type": "text", "text": "pong" }],
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
    .to_string()
}

fn openai_body(model: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "pong" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string()
}

#[tokio::test]
async fn adapters_record_provider_reported_rate_limits_on_success_and_on_rejection() {
    if std::net::TcpListener::bind("127.0.0.1:0").is_err() {
        return;
    }
    let store = std::env::temp_dir().join(format!(
        "rtrt-ratelimit-adapters-{}-{}.tsv",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    // SAFETY: single-threaded at this point (one test fn in this binary), and
    // the var is only read by this crate's rate-limit path resolution.
    unsafe { std::env::set_var("RTRT_PROVIDER_RATELIMIT_PATH", &store) };

    // 1. Anthropic, 200: the `anthropic-ratelimit-*` headers are captured under
    //    the pool the call drew from (a prefix-less model is unpooled).
    let mut anthropic_server = mockito::Server::new_async().await;
    let _ok = anthropic_server
        .mock("POST", "/messages")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("anthropic-ratelimit-requests-limit", "1000")
        .with_header("anthropic-ratelimit-requests-remaining", "994")
        .with_header("anthropic-ratelimit-tokens-limit", "80000")
        .with_header("anthropic-ratelimit-tokens-remaining", "61234")
        .with_header("anthropic-ratelimit-tokens-reset", "6m0s")
        .with_body(anthropic_body())
        .create_async()
        .await;
    AnthropicProvider::new("test")
        .with_base_url(anthropic_server.url())
        .chat(req("claude-haiku-4-5"))
        .await
        .expect("anthropic chat");

    let signals = rate_limit_signals();
    let anthropic = signals.get("anthropic").expect("anthropic signal recorded");
    assert_eq!(anthropic.status, 200);
    assert_eq!(anthropic.requests.limit, Some(1_000));
    assert_eq!(anthropic.requests.remaining, Some(994));
    assert_eq!(anthropic.tokens.remaining, Some(61_234));
    assert!(
        anthropic.tokens.reset_at.expect("reset recorded") > anthropic.observed_at,
        "a 6m window must resolve to an instant in the future"
    );
    assert_eq!(
        anthropic.requests.reset_at, None,
        "an unreported reset stays absent, never borrowed from the other axis"
    );

    // 2. OpenAI, 429: exactly when the numbers matter. The rejection still
    //    reaches the caller unchanged.
    let mut openai_server = mockito::Server::new_async().await;
    let _throttled = openai_server
        .mock("POST", "/chat/completions")
        .with_status(429)
        .with_header("content-type", "application/json")
        .with_header("x-ratelimit-limit-requests", "500")
        .with_header("x-ratelimit-remaining-requests", "0")
        .with_header("x-ratelimit-reset-requests", "30s")
        .with_header("retry-after", "30")
        .with_body(r#"{"error":{"message":"rate limit"}}"#)
        .create_async()
        .await;
    let err = OpenAIProvider::new("test")
        .with_base_url(openai_server.url())
        .chat(req("gpt-5.4-mini"))
        .await
        .expect_err("a 429 must still surface as an error");
    assert!(
        err.to_string().contains("429"),
        "recording must not change what the caller sees: {err}"
    );

    let signals = rate_limit_signals();
    let openai = signals.get("openai").expect("429 signal recorded");
    assert_eq!(openai.status, 429);
    assert_eq!(openai.requests.remaining, Some(0));
    assert_eq!(
        openai.backoff_until(),
        Some(openai.observed_at + 30),
        "retry-after resolves to the instant it points at"
    );

    // 3. An OpenAI-compatible server gets its OWN quota bucket, keyed by the
    //    adapter's name plus the model's pool prefix — never OpenAI's.
    let mut local_server = mockito::Server::new_async().await;
    let _local = local_server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_header("x-ratelimit-limit-tokens", "4096")
        .with_header("x-ratelimit-remaining-tokens", "4000")
        .with_body(openai_body("qwen3/32b"))
        .create_async()
        .await;
    OpenAICompatibleProvider::new("vllm", local_server.url())
        .chat(req("qwen3/32b"))
        .await
        .expect("local chat");

    let signals = rate_limit_signals();
    assert!(
        signals.contains_key("vllm#qwen3"),
        "a local server is its own pool, got {:?}",
        signals.keys().collect::<Vec<_>>()
    );
    assert_eq!(signals["vllm#qwen3"].tokens.remaining, Some(4_000));

    // 4. A response that discloses nothing leaves nothing behind.
    let before = std::fs::read_to_string(&store).unwrap_or_default();
    let mut silent_server = mockito::Server::new_async().await;
    let _silent = silent_server
        .mock("POST", "/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(openai_body("gpt-5.4-mini"))
        .create_async()
        .await;
    OpenAIProvider::new("test")
        .with_base_url(silent_server.url())
        .chat(req("gpt-5.4-mini"))
        .await
        .expect("silent chat");
    assert_eq!(
        std::fs::read_to_string(&store).unwrap_or_default(),
        before,
        "no rate-limit headers must mean no row at all"
    );

    let _ = std::fs::remove_file(&store);
}
