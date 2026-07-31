//! OpenAI-compatible HTTP gateway — turn any OpenAI client into an rtrt client.
//!
//! Exposes an axum server speaking the OpenAI Chat Completions wire format so
//! ANY tool that can point at an OpenAI base URL (`OPENAI_BASE_URL` /
//! `--base-url` / SDK `baseURL`) becomes an rtrt front-end with one env var:
//!
//! ```text
//! POST /v1/chat/completions   OpenAI request/response (+ SSE when stream:true)
//! GET  /v1/models             routable targets / pseudo-models
//! GET  /healthz               liveness probe
//! ```
//!
//! ## Routing semantics for the `model` field
//!
//! - `auto` / `""` / `rtrt/auto` — full route: infer a [`Capability`] from the
//!   request (code fence → code, long → reasoning, else general chat), then
//!   [`select_route`] over the detected tools with the ledger-overlaid
//!   [`UsageSnapshot::load_for_routing`] snapshot, and walk
//!   [`RouteDecision::ranked_targets`] with [`invoke_with_failover`].
//! - `rtrt/cheapest` / `rtrt/best` — same ranked list, but the cheapest cost
//!   tier (`Prefer::Cheapest`) or the highest-capability tier
//!   (`Prefer::Quality`).
//! - Explicit provider-prefixed (`anthropic/claude-…`, `openai/gpt-…`,
//!   `ollama/…`) or a bare model id — dispatched through the existing
//!   [`Gateway`] prefix path (the same brains the dashboard / MCP use), which
//!   records to the usage ledger on every call.
//!
//! Every dispatch records to the usage ledger through the machinery it reuses,
//! so there is no second accounting system and no double-recording.
//!
//! ## Limitations (honest)
//!
//! This is a **text-only** bridge: tool-calling / function-calling / vision
//! content from the client is not passed through. `rtrt/team` is genuinely
//! incremental for Claude CLI text deltas and filters transport/tool metadata;
//! legacy auto/explicit routes remain buffered before SSE emission.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{FromRequest, Request, State},
    http::{HeaderValue, StatusCode, header::AUTHORIZATION},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use rtrt_core::{Capability, Error, Result};
use serde::{Deserialize, Serialize};

use crate::{
    ChatMessage, ChatRequest, Gateway, Prefer, Role, RouteRequest, TeamExecutionContext,
    UsageSnapshot, dispatch_team, dispatch_team_stream_with_context, invoke, invoke_with_failover,
    is_retryable_error, select_route, usage_ledger,
};

/// Requests longer than this many characters are treated as reasoning work
/// (long context / analysis) when no code fence is present. A heuristic
/// classification nudge for capability selection, NOT a budget cap — it only
/// biases which tier `auto` prefers and is documented in USAGE.md.
const REASONING_CHAR_THRESHOLD: usize = 2000;
const OPENCODE_SESSION_HEADER: &str = "x-rtrt-opencode-session";
const OPENCODE_SERVER_HEADER: &str = "x-rtrt-opencode-server";

/// Default bind port for `rtrt gateway serve`.
pub const DEFAULT_GATEWAY_PORT: u16 = 7412;
/// Default bind host — loopback, so the endpoint is not world-reachable unless
/// the operator explicitly opts into a wider bind.
pub const DEFAULT_GATEWAY_HOST: &str = "127.0.0.1";

// ---------------------------------------------------------------------------
// Model-string routing
// ---------------------------------------------------------------------------

/// How the `model` field of an OpenAI request maps onto rtrt routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelRoute {
    /// Deterministic team router: usage/order only, no intermediate LLM.
    Team,
    /// Full headroom-aware route, cheapest-first.
    Auto,
    /// Full route, cheapest cost tier.
    Cheapest,
    /// Full route, highest-capability (quality) tier.
    Best,
    /// A specific model dispatched through the [`Gateway`] prefix path. `model`
    /// is the id passed to the gateway (any `provider/` prefix stripped).
    Explicit { model: String },
}

impl ModelRoute {
    /// Parse the OpenAI `model` field. Empty / `auto` / `rtrt/auto` route
    /// automatically; `rtrt/cheapest` and `rtrt/best` (and their bare
    /// `cheapest` / `best` aliases) pick a tier; anything else is explicit and
    /// its leading `provider/` segment (if any) is stripped for gateway
    /// prefix matching.
    pub fn parse(model: &str) -> Self {
        let trimmed = model.trim();
        let lower = trimmed.to_ascii_lowercase();
        match lower.as_str() {
            "team" | "rtrt/team" => return Self::Team,
            "" | "auto" | "rtrt/auto" | "rtrt" => return Self::Auto,
            "cheapest" | "rtrt/cheapest" => return Self::Cheapest,
            "best" | "rtrt/best" => return Self::Best,
            _ => {}
        }
        // `provider/model` → strip the provider hint and dispatch by the
        // model id (the gateway matches providers by model-id prefix). Split on
        // the first slash only, so ollama tags like `library/model:tag` keep
        // their suffix intact.
        let model = match trimmed.split_once('/') {
            Some((_provider, rest)) if !rest.is_empty() => rest.to_string(),
            _ => trimmed.to_string(),
        };
        Self::Explicit { model }
    }

    /// The routing preference for the auto family, or `None` for explicit
    /// models (which never enter the router).
    fn prefer(&self) -> Option<Prefer> {
        match self {
            Self::Auto | Self::Cheapest => Some(Prefer::Cheapest),
            Self::Best => Some(Prefer::Quality),
            Self::Team | Self::Explicit { .. } => None,
        }
    }
}

/// Infer the needed [`Capability`] from the request body. Deliberately simple
/// and documented: a fenced code block asks for code, an otherwise long body
/// asks for reasoning, and anything else is general chat (no capability
/// filter).
pub fn infer_capability(messages: &[ChatMessage]) -> Option<Capability> {
    let has_code = messages.iter().any(|m| m.content.contains("```"));
    if has_code {
        return Some(Capability::Code);
    }
    let total: usize = messages.iter().map(|m| m.content.chars().count()).sum();
    if total > REASONING_CHAR_THRESHOLD {
        return Some(Capability::Reasoning);
    }
    None
}

/// Flatten a chat transcript into a single prompt string for the routed
/// (invoke) path. A lone user turn is passed through verbatim; multi-turn
/// conversations are rendered as labelled turns so local models keep context.
fn flatten_messages(messages: &[ChatMessage]) -> String {
    if let [only] = messages
        && only.role == Role::User
    {
        return only.content.clone();
    }
    let mut out = String::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        let label = match m.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        out.push_str(label);
        out.push_str(": ");
        out.push_str(&m.content);
    }
    out
}

fn team_conversation_prompt(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User && !message.content.trim().is_empty())
        .map(|message| message.content.clone())
}

// ---------------------------------------------------------------------------
// OpenAI wire types
// ---------------------------------------------------------------------------

/// OpenAI message content: either a plain string or an array of typed parts
/// (`{"type":"text","text":"…"}`). We collapse parts to their text so common
/// SDKs that always send array content still work; non-text parts are dropped
/// (this is a text-only bridge).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(default)]
    pub text: Option<String>,
}

impl MessageContent {
    fn into_text(self) -> String {
        match self {
            Self::Text(s) => s,
            Self::Parts(parts) => parts
                .into_iter()
                .filter_map(|p| p.text)
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage {
    pub role: String,
    // `Option` so an explicit `content: null` (which OpenAI SDKs serialize on
    // assistant tool-call turns, e.g. `{"role":"assistant","content":null,
    // "tool_calls":[…]}`) parses as `None` instead of a 422. `#[serde(default)]`
    // alone only covers a MISSING key, not an explicit null — a tool-using
    // client replaying history would otherwise be hard-blocked at JSON parse.
    #[serde(default)]
    pub content: Option<MessageContent>,
}

/// Incoming `POST /v1/chat/completions` body (the fields we honour).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    #[serde(default)]
    pub model: String,
    pub messages: Vec<WireMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}

impl ChatCompletionRequest {
    /// Convert the wire messages into internal [`ChatMessage`]s (roles other
    /// than system/assistant map to user, matching lenient OpenAI behaviour).
    fn to_chat_messages(&self) -> Vec<ChatMessage> {
        self.messages
            .iter()
            .cloned()
            .map(|m| ChatMessage {
                role: match m.role.as_str() {
                    "system" | "developer" => Role::System,
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                },
                // A null / omitted content collapses to an empty string.
                content: m.content.map(MessageContent::into_text).unwrap_or_default(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct UsageWire {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl UsageWire {
    fn new(prompt: u64, completion: u64) -> Self {
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt.saturating_add(completion),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: UsageWire,
    /// rtrt extension: which detected target actually served the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtrt_target: Option<String>,
}

// Streaming chunk types (`chat.completion.chunk`).

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelObject {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub owned_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelObject>,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The result of a unified dispatch: the assistant text plus the accounting we
/// can honestly report back on the wire.
struct Completion {
    content: String,
    usage: UsageWire,
    target: String,
}

/// Run one chat request through the routing brains selected by `route`.
///
/// The auto family flattens the transcript to a prompt and walks the
/// headroom-aware ranked list with failover; explicit models go straight
/// through the shared [`Gateway`] prefix path. Both surfaces record to the
/// usage ledger via the machinery they reuse.
async fn dispatch(
    state: &GatewayState,
    route: ModelRoute,
    req: &ChatCompletionRequest,
) -> Result<Completion> {
    let messages = req.to_chat_messages();
    match route {
        ModelRoute::Team => {
            let prompt = team_conversation_prompt(&messages).ok_or_else(|| {
                Error::Provider("team route requires a non-empty user message".to_string())
            })?;
            let config = rtrt_core::Config::load_effective_for_cwd();
            let outcome = dispatch_team(&config.team, &prompt, state.timeout).await?;
            let prompt_tokens = usage_ledger::estimate_tokens(&prompt);
            let completion_tokens = usage_ledger::estimate_tokens(&outcome.outcome.output);
            Ok(Completion {
                content: outcome.outcome.output,
                usage: UsageWire::new(prompt_tokens, completion_tokens),
                target: outcome.outcome.target,
            })
        }
        ModelRoute::Explicit { model } => {
            let chat = ChatRequest {
                model: model.clone(),
                messages,
                max_tokens: req.max_tokens,
                temperature: req.temperature,
            };
            let resp = state.gateway.chat(chat).await?;
            Ok(Completion {
                usage: UsageWire::new(resp.usage.input_tokens, resp.usage.output_tokens),
                target: resp.provider,
                content: resp.content,
            })
        }
        auto => {
            let prefer = auto.prefer().unwrap_or(Prefer::Cheapest);
            let prompt = flatten_messages(&messages);
            let capability = infer_capability(&messages);
            let cfg = rtrt_core::Config::load_effective_for_cwd();
            let tools = rtrt_core::detect_tools_with_config(cfg);
            let usage = UsageSnapshot::load_for_routing();
            let decision = select_route(
                &RouteRequest {
                    capability,
                    prefer,
                    target: None,
                    model: None,
                    mode: None,
                },
                &tools,
                &usage,
            )?;
            let ranked = decision.ranked_targets();
            let outcome = invoke_with_failover(&ranked, &prompt, state.timeout).await?;
            // The routed path does not surface exact token counts, so estimate
            // both sides from text (chars/4) for the wire `usage` block — the
            // ledger already recorded the authoritative row inside
            // `invoke_agent`.
            let prompt_tokens = usage_ledger::estimate_tokens(&prompt);
            let completion_tokens = usage_ledger::estimate_tokens(&outcome.outcome.output);
            Ok(Completion {
                content: outcome.outcome.output,
                usage: UsageWire::new(prompt_tokens, completion_tokens),
                target: outcome.outcome.target,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Server state + router
// ---------------------------------------------------------------------------

/// Shared state for the gateway axum app.
#[derive(Clone)]
pub struct GatewayState {
    token: Option<Arc<String>>,
    timeout: Duration,
    gateway: Arc<Gateway>,
}

impl GatewayState {
    /// Production state: explicit-model dispatch flows through a
    /// [`Gateway::from_env`] (ledger recording on, real provider keys).
    pub fn from_env(token: Option<String>, timeout: Duration) -> Self {
        Self {
            token: token.map(Arc::new),
            timeout,
            gateway: Arc::new(Gateway::from_env()),
        }
    }

    /// Test / embedding state with a caller-supplied gateway (e.g. an Echo
    /// provider), so the explicit path can be exercised without real keys.
    pub fn with_gateway(gateway: Arc<Gateway>, token: Option<String>, timeout: Duration) -> Self {
        Self {
            token: token.map(Arc::new),
            timeout,
            gateway,
        }
    }
}

/// Build the gateway axum [`Router`] for `state`. Exposed so tests can drive it
/// with an injected gateway.
pub fn app(state: GatewayState) -> Router {
    let token = state.token.clone();
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .route("/healthz", get(healthz))
        .layer(axum::middleware::from_fn(move |req, next| {
            let token = token.clone();
            async move { bearer_guard(token, req, next).await }
        }))
        .with_state(state)
}

/// Bind and serve the gateway. Loopback by default; a non-loopback bind without
/// a bearer token logs a warning (the endpoint would then be unauthenticated).
pub async fn serve(
    host: &str,
    port: u16,
    token: Option<String>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let is_loopback =
        host.starts_with("127.") || host == "::1" || host == "[::1]" || host == "localhost";
    if token.is_none() && !is_loopback {
        tracing::warn!(
            "binding {host}:{port} without a bearer token is risky; non-loopback callers can hit the gateway unauthenticated (set RTRT_GATEWAY_TOKEN)."
        );
    }
    let state = GatewayState::from_env(token, timeout);
    let app = app(state);
    let bind = format!("{host}:{port}");
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "address {bind} is already in use. Free the port or pass --port to choose another."
            );
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!("rtrt gateway listening on http://{bind}/v1 (OpenAI-compatible)");
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

/// Body extractor that renders EVERY parse failure as the OpenAI error envelope
/// (`{"error":{message,type}}`) with a 400. The stock `Json` extractor leaks
/// axum's own rejections — plain-text 400 for malformed JSON, 422 for a missing
/// field, 415 for a missing `Content-Type` — none of which a real OpenAI client
/// expects on this route.
struct OpenAiRequest {
    request: ChatCompletionRequest,
    team_context: TeamExecutionContext,
}

impl<S> FromRequest<S> for OpenAiRequest
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, Self::Rejection> {
        let team_context = team_context_from_headers(req.headers());
        match Json::<ChatCompletionRequest>::from_request(req, state).await {
            Ok(Json(request)) => Ok(Self {
                request,
                team_context,
            }),
            Err(rejection) => Err(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &rejection.body_text(),
            )),
        }
    }
}

async fn chat_completions(State(state): State<GatewayState>, request: OpenAiRequest) -> Response {
    let OpenAiRequest {
        request: req,
        team_context,
    } = request;
    if req.messages.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must not be empty",
        );
    }
    let route = ModelRoute::parse(&req.model);
    let model_label = if req.model.trim().is_empty() {
        "rtrt/auto".to_string()
    } else {
        req.model.clone()
    };
    let stream = req.stream;
    if stream && route == ModelRoute::Team {
        return match team_stream_response(&state, &model_label, &req, team_context) {
            Ok(response) => response,
            Err(error) => dispatch_error_response(&error),
        };
    }
    match dispatch(&state, route, &req).await {
        Ok(completion) if stream => stream_response(&model_label, completion),
        Ok(completion) => Json(completion_response(&model_label, completion)).into_response(),
        Err(e) => dispatch_error_response(&e),
    }
}

fn team_stream_response(
    state: &GatewayState,
    model: &str,
    req: &ChatCompletionRequest,
    context: TeamExecutionContext,
) -> Result<Response> {
    let messages = req.to_chat_messages();
    let prompt = team_conversation_prompt(&messages).ok_or_else(|| {
        Error::Provider("team route requires a non-empty user message".to_string())
    })?;
    let config = rtrt_core::Config::load_effective_for_cwd();
    let stream = dispatch_team_stream_with_context(&config.team, &prompt, state.timeout, context)?;
    let id = completion_id();
    let created = now_epoch_secs();
    let model = model.to_string();

    let role = ChatCompletionChunk {
        id: id.clone(),
        object: "chat.completion.chunk".to_string(),
        created,
        model: model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: Delta {
                role: Some("assistant".to_string()),
                content: None,
            },
            finish_reason: None,
        }],
    };
    let role_event = stream::once(async move { Ok::<Event, Infallible>(chunk_event(&role)) });
    let delta_id = id.clone();
    let delta_model = model.clone();
    let deltas = stream.map(move |item| {
        let event = match item {
            Ok(text) => chunk_event(&ChatCompletionChunk {
                id: delta_id.clone(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: delta_model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: Delta {
                        role: None,
                        content: Some(text),
                    },
                    finish_reason: None,
                }],
            }),
            Err(error) => Event::default().event("error").data(
                serde_json::json!({
                    "error": {
                        "type": "upstream_error",
                        "message": error.to_string()
                    }
                })
                .to_string(),
            ),
        };
        Ok::<Event, Infallible>(event)
    });
    let final_chunk = ChatCompletionChunk {
        id,
        object: "chat.completion.chunk".to_string(),
        created,
        model,
        choices: vec![ChunkChoice {
            index: 0,
            delta: Delta::default(),
            finish_reason: Some("stop".to_string()),
        }],
    };
    let end = stream::iter([
        Ok::<Event, Infallible>(chunk_event(&final_chunk)),
        Ok::<Event, Infallible>(Event::default().data("[DONE]")),
    ]);
    Ok(Sse::new(role_event.chain(deltas).chain(end))
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn team_context_from_headers(headers: &axum::http::HeaderMap) -> TeamExecutionContext {
    let session_id = headers
        .get(OPENCODE_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .map(str::to_string);
    let server_url = headers
        .get(OPENCODE_SERVER_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| reqwest::Url::parse(value).ok())
        .filter(|url| {
            url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1"))
        })
        .map(|url| url.to_string().trim_end_matches('/').to_string());
    if session_id.is_some() && server_url.is_some() {
        TeamExecutionContext {
            opencode_session_id: session_id,
            opencode_server_url: server_url,
        }
    } else {
        TeamExecutionContext::default()
    }
}

/// Map a dispatch [`Error`] onto the right HTTP status + OpenAI error shape.
///
/// A permanent client error (unknown model → "no provider registered") returns
/// a non-retryable 404 `model_not_found`, so SDKs with `max_retries` do not
/// storm a typo (which on the `auto` route would re-invoke ranked CLIs and burn
/// real tokens). Other terminal/config errors (auth, disabled target, no route)
/// return a non-retryable 400. Only genuinely transient failures (rate-limit /
/// 5xx / timeout, which `is_retryable_error` recognises) keep the 502 that
/// invites a retry.
fn dispatch_error_response(e: &Error) -> Response {
    let msg = e.to_string();
    if msg.to_ascii_lowercase().contains("no provider registered") {
        return error_response_code(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            &msg,
        );
    }
    if is_retryable_error(e) {
        return error_response(StatusCode::BAD_GATEWAY, "upstream_error", &msg);
    }
    error_response(StatusCode::BAD_REQUEST, "invalid_request_error", &msg)
}

fn completion_response(model: &str, c: Completion) -> ChatCompletionResponse {
    ChatCompletionResponse {
        id: completion_id(),
        object: "chat.completion".to_string(),
        created: now_epoch_secs(),
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: ResponseMessage {
                role: "assistant".to_string(),
                content: c.content,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: c.usage,
        rtrt_target: Some(c.target),
    }
}

/// Emit a buffered completion as SSE `chat.completion.chunk`s: one chunk with
/// the role + full content, a terminal chunk carrying `finish_reason:"stop"`,
/// then the `[DONE]` sentinel. Honest but wire-compatible — CLI-mode targets
/// only produce full text, so there is nothing to stream token-by-token.
fn stream_response(model: &str, c: Completion) -> Response {
    let id = completion_id();
    let created = now_epoch_secs();
    let content_chunk = ChatCompletionChunk {
        id: id.clone(),
        object: "chat.completion.chunk".to_string(),
        created,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: Delta {
                role: Some("assistant".to_string()),
                content: Some(c.content),
            },
            finish_reason: None,
        }],
    };
    let final_chunk = ChatCompletionChunk {
        id,
        object: "chat.completion.chunk".to_string(),
        created,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: Delta::default(),
            finish_reason: Some("stop".to_string()),
        }],
    };
    let events = vec![
        chunk_event(&content_chunk),
        chunk_event(&final_chunk),
        Event::default().data("[DONE]"),
    ];
    let body = stream::iter(events.into_iter().map(Ok::<Event, Infallible>));
    Sse::new(body)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn chunk_event(chunk: &ChatCompletionChunk) -> Event {
    // Serialization of our own owned struct cannot fail; fall back to an empty
    // object rather than panicking inside the stream.
    let data = serde_json::to_string(chunk).unwrap_or_else(|_| "{}".to_string());
    Event::default().data(data)
}

async fn list_models(State(_state): State<GatewayState>) -> Json<ModelsResponse> {
    let created = now_epoch_secs();
    let mut data = vec![
        model_object("rtrt/team", "rtrt", created),
        model_object("rtrt/auto", "rtrt", created),
        model_object("rtrt/cheapest", "rtrt", created),
        model_object("rtrt/best", "rtrt", created),
    ];
    let cfg = rtrt_core::Config::load_effective_for_cwd();
    for tool in rtrt_core::detect_tools_with_config(cfg) {
        if !tool.installed || !tool.enabled {
            continue;
        }
        if tool.models.is_empty() {
            data.push(model_object(&tool.name, &tool.name, created));
        } else {
            for model in &tool.models {
                data.push(model_object(
                    &format!("{}/{}", tool.name, model),
                    &tool.name,
                    created,
                ));
            }
        }
    }
    Json(ModelsResponse {
        object: "list".to_string(),
        data,
    })
}

fn model_object(id: &str, owned_by: &str, created: u64) -> ModelObject {
    ModelObject {
        id: id.to_string(),
        object: "model".to_string(),
        created,
        owned_by: owned_by.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Errors + auth + small helpers
// ---------------------------------------------------------------------------

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    error_body(status, kind, None, message)
}

/// Error envelope carrying an OpenAI `code` (e.g. `model_not_found`).
fn error_response_code(status: StatusCode, kind: &str, code: &str, message: &str) -> Response {
    error_body(status, kind, Some(code), message)
}

fn error_body(status: StatusCode, kind: &str, code: Option<&str>, message: &str) -> Response {
    let mut err = serde_json::json!({
        "message": message,
        "type": kind,
    });
    if let Some(code) = code {
        err["code"] = serde_json::Value::String(code.to_string());
    }
    let body = serde_json::json!({ "error": err });
    (status, Json(body)).into_response()
}

/// Bearer-token guard. `/healthz` is always open (liveness probes carry no
/// auth); everything else requires `Authorization: Bearer <token>` when a
/// token is configured. Comparison is constant-time.
async fn bearer_guard(
    expected: Option<Arc<String>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }
    let Some(expected) = expected else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.strip_prefix("Bearer "))
        .map(str::to_string);
    let ok = presented
        .as_deref()
        .is_some_and(|tok| constant_time_eq(tok.as_bytes(), expected.as_bytes()));
    if ok {
        return next.run(req).await;
    }
    let mut resp = error_response(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "bearer token missing or invalid",
    );
    resp.headers_mut().insert(
        "WWW-Authenticate",
        HeaderValue::from_static("Bearer realm=\"rtrt-gateway\""),
    );
    resp
}

/// Constant-time byte comparison — same shape as the dashboard / MCP guards.
/// A length mismatch short-circuits, so token length is not perfectly hidden,
/// but the byte content never leaks through early-exit timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A unique-ish `chatcmpl-…` id from the clock plus a process-local counter.
fn completion_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("chatcmpl-{:x}{:04x}", now_epoch_secs(), n & 0xffff)
}

/// The default per-invocation timeout for the routed path.
pub fn default_timeout() -> Duration {
    Duration::from_secs(invoke::DEFAULT_TIMEOUT_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatResponse, Provider, Usage};
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::http::Request;
    use rtrt_core::{CostClass, DetectedTool, InvocationMode, ToolKind};
    use tower::ServiceExt; // oneshot

    #[test]
    fn model_route_parses_auto_family_and_aliases() {
        assert_eq!(ModelRoute::parse("team"), ModelRoute::Team);
        assert_eq!(ModelRoute::parse("rtrt/team"), ModelRoute::Team);
        assert_eq!(ModelRoute::parse(""), ModelRoute::Auto);
        assert_eq!(ModelRoute::parse("auto"), ModelRoute::Auto);
        assert_eq!(ModelRoute::parse("rtrt/auto"), ModelRoute::Auto);
        assert_eq!(ModelRoute::parse("RTRT/AUTO"), ModelRoute::Auto);
        assert_eq!(ModelRoute::parse("cheapest"), ModelRoute::Cheapest);
        assert_eq!(ModelRoute::parse("rtrt/cheapest"), ModelRoute::Cheapest);
        assert_eq!(ModelRoute::parse("best"), ModelRoute::Best);
        assert_eq!(ModelRoute::parse("rtrt/best"), ModelRoute::Best);
    }

    #[test]
    fn model_route_strips_provider_prefix_for_explicit_models() {
        assert_eq!(
            ModelRoute::parse("anthropic/claude-haiku-4-5"),
            ModelRoute::Explicit {
                model: "claude-haiku-4-5".to_string()
            }
        );
        assert_eq!(
            ModelRoute::parse("openai/gpt-5.4-mini"),
            ModelRoute::Explicit {
                model: "gpt-5.4-mini".to_string()
            }
        );
        // Bare model id: no prefix to strip.
        assert_eq!(
            ModelRoute::parse("gpt-4o"),
            ModelRoute::Explicit {
                model: "gpt-4o".to_string()
            }
        );
        // Ollama tag survives the split.
        assert_eq!(
            ModelRoute::parse("ollama/qwen2.5-coder:7b"),
            ModelRoute::Explicit {
                model: "qwen2.5-coder:7b".to_string()
            }
        );
    }

    #[test]
    fn prefer_maps_tiers_correctly() {
        assert_eq!(ModelRoute::Auto.prefer(), Some(Prefer::Cheapest));
        assert_eq!(ModelRoute::Cheapest.prefer(), Some(Prefer::Cheapest));
        assert_eq!(ModelRoute::Best.prefer(), Some(Prefer::Quality));
        assert_eq!(ModelRoute::Team.prefer(), None);
        assert_eq!(
            ModelRoute::Explicit {
                model: "x".to_string()
            }
            .prefer(),
            None
        );
    }

    #[test]
    fn team_route_forwards_only_latest_user_message_verbatim() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "ignore router harness".to_string(),
            },
            ChatMessage {
                role: Role::User,
                content: "first".to_string(),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "answer".to_string(),
            },
            ChatMessage {
                role: Role::User,
                content: "  UTF-8 한글\nexact  ".to_string(),
            },
        ];

        assert_eq!(
            team_conversation_prompt(&messages).as_deref(),
            Some("  UTF-8 한글\nexact  ")
        );
    }

    #[test]
    fn single_user_team_route_is_byte_preserved() {
        let original = "  UTF-8 한글\r\nexact  ";
        let messages = vec![ChatMessage {
            role: Role::User,
            content: original.to_string(),
        }];

        assert_eq!(
            team_conversation_prompt(&messages).as_deref(),
            Some(original)
        );
    }

    #[test]
    fn opencode_context_accepts_only_valid_session_and_loopback_server() {
        let mut headers = HeaderMap::new();
        headers.insert(OPENCODE_SESSION_HEADER, "ses_native-123".parse().unwrap());
        headers.insert(
            OPENCODE_SERVER_HEADER,
            "http://127.0.0.1:4096/".parse().unwrap(),
        );

        assert_eq!(
            team_execution_context(&headers),
            TeamExecutionContext {
                opencode_session_id: Some("ses_native-123".to_string()),
                opencode_server_url: Some("http://127.0.0.1:4096/".to_string()),
            }
        );

        headers.insert(
            OPENCODE_SERVER_HEADER,
            "https://example.com/".parse().unwrap(),
        );
        assert_eq!(
            team_execution_context(&headers),
            TeamExecutionContext::default()
        );
    }

    fn user(content: &str) -> ChatMessage {
        ChatMessage {
            role: Role::User,
            content: content.to_string(),
        }
    }

    #[test]
    fn capability_inference_heuristic() {
        assert_eq!(infer_capability(&[user("hello")]), None);
        assert_eq!(
            infer_capability(&[user("fix this ```rust\nfn main(){}\n```")]),
            Some(Capability::Code)
        );
        let long = "word ".repeat(REASONING_CHAR_THRESHOLD);
        assert_eq!(
            infer_capability(&[user(&long)]),
            Some(Capability::Reasoning)
        );
    }

    #[test]
    fn flatten_single_user_message_is_verbatim() {
        assert_eq!(flatten_messages(&[user("just this")]), "just this");
    }

    #[test]
    fn flatten_multi_turn_labels_roles() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "be terse".to_string(),
            },
            user("hi"),
        ];
        assert_eq!(flatten_messages(&messages), "System: be terse\n\nUser: hi");
    }

    #[test]
    fn request_wire_deserializes_string_and_array_content() {
        let raw = r#"{
            "model": "auto",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": [{"type":"text","text":"a"},{"type":"text","text":"b"}]}
            ],
            "stream": true,
            "max_tokens": 64
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(raw).expect("parse");
        assert!(req.stream);
        assert_eq!(req.max_tokens, Some(64));
        let msgs = req.to_chat_messages();
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content, "sys");
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[1].content, "ab");
    }

    #[test]
    fn request_wire_tolerates_null_and_missing_content() {
        // OpenAI SDKs serialize assistant tool-call turns with `content: null`;
        // a follow-up turn may omit content entirely. Both must parse (as an
        // empty string) rather than 422.
        let raw = r#"{
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "content": null},
                {"role": "assistant"},
                {"role": "user", "content": "ping"}
            ]
        }"#;
        let req: ChatCompletionRequest = serde_json::from_str(raw).expect("parse");
        let msgs = req.to_chat_messages();
        assert_eq!(msgs[0].content, "");
        assert_eq!(msgs[1].content, "");
        assert_eq!(msgs[2].content, "ping");
    }

    #[test]
    fn completion_response_round_trips() {
        let resp = completion_response(
            "rtrt/auto",
            Completion {
                content: "hi there".to_string(),
                usage: UsageWire::new(3, 2),
                target: "ollama".to_string(),
            },
        );
        let json = serde_json::to_string(&resp).expect("serialize");
        let back: ChatCompletionResponse = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.object, "chat.completion");
        assert_eq!(back.choices[0].message.content, "hi there");
        assert_eq!(back.usage.total_tokens, 5);
        assert_eq!(back.rtrt_target.as_deref(), Some("ollama"));
    }

    #[test]
    fn routing_decision_selection_with_mocked_usage() {
        // openai is near-exhausted against its cap; anthropic is roomy. The
        // gateway's auto route must prefer the roomier peer within the tier.
        let tools = vec![
            detected("anthropic", CostClass::ApiMetered),
            detected("openai", CostClass::ApiMetered),
        ];
        let usage = UsageSnapshot::from_usage_and_limits_for_tests(
            [("anthropic", 10), ("openai", 95)],
            [("anthropic", 100), ("openai", 100)],
        );
        let req = RouteRequest {
            capability: infer_capability(&[user("hello")]),
            prefer: ModelRoute::Auto.prefer().unwrap(),
            target: None,
            model: None,
            mode: None,
        };
        let decision = select_route(&req, &tools, &usage).expect("route");
        assert_eq!(decision.target, "anthropic");
    }

    // --- End-to-end axum test hitting the router with an Echo gateway ---

    struct Echo;

    #[async_trait]
    impl Provider for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn supported_models(&self) -> &[&'static str] {
            &[]
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
            let last = req
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(ChatResponse {
                provider: "echo".to_string(),
                model: req.model,
                content: format!("echo: {last}"),
                usage: Usage {
                    input_tokens: 4,
                    output_tokens: 6,
                    ..Default::default()
                },
            })
        }
    }

    fn echo_state(token: Option<String>) -> GatewayState {
        // Register Echo as the default provider so any explicit model reaches
        // it. Ledger recording stays off (Gateway::new default), so the test
        // never writes to ~/.rtrt.
        let gateway = Gateway::new()
            .register("echo", Box::new(Echo), [] as [&'static str; 0])
            .with_default_last();
        GatewayState::with_gateway(Arc::new(gateway), token, Duration::from_secs(5))
    }

    #[tokio::test]
    async fn healthz_ok() {
        let app = app(echo_state(None));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn explicit_model_completes_through_gateway() {
        let app = app(echo_state(None));
        let body = serde_json::json!({
            "model": "openai/gpt-4o",
            "messages": [{"role": "user", "content": "ping"}]
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let parsed: ChatCompletionResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.choices[0].message.content, "echo: ping");
        assert_eq!(parsed.usage.total_tokens, 10);
        assert_eq!(parsed.rtrt_target.as_deref(), Some("echo"));
    }

    #[tokio::test]
    async fn streaming_emits_chunks_and_done() {
        let app = app(echo_state(None));
        let body = serde_json::json!({
            "model": "gpt-4o",
            "stream": true,
            "messages": [{"role": "user", "content": "ping"}]
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ctype.starts_with("text/event-stream"), "got {ctype}");
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("chat.completion.chunk"), "body: {text}");
        assert!(text.contains("echo: ping"), "body: {text}");
        assert!(text.contains("data: [DONE]"), "body: {text}");
    }

    #[tokio::test]
    async fn bearer_guard_rejects_and_accepts() {
        let app = app(echo_state(Some("secret".to_string())));
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "ping"}]
        });
        // No token → 401.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // Correct token → 200.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer secret")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_messages_is_bad_request() {
        let app = app(echo_state(None));
        let body = serde_json::json!({ "model": "auto", "messages": [] });
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Echo gateway with a model-id prefix but NO default, so an unmatched
    /// model surfaces the gateway's "no provider registered" error.
    fn echo_state_no_default() -> GatewayState {
        let gateway = Gateway::new().register("openai", Box::new(Echo), ["gpt-"]);
        GatewayState::with_gateway(Arc::new(gateway), None, Duration::from_secs(5))
    }

    /// POST a raw body (optionally without a content-type) and return the
    /// response status plus the parsed JSON body.
    async fn post_raw(
        state: GatewayState,
        body: &str,
        content_type: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions");
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        let resp = app(state)
            .oneshot(
                builder
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    fn assert_error_envelope(value: &serde_json::Value, expected_type: &str) {
        let err = value
            .get("error")
            .unwrap_or_else(|| panic!("expected error envelope, got: {value}"));
        assert!(err.get("message").and_then(|m| m.as_str()).is_some());
        assert_eq!(
            err.get("type").and_then(|t| t.as_str()),
            Some(expected_type)
        );
    }

    #[tokio::test]
    async fn null_content_completes_200() {
        // Bug #1: a tool-history turn with `content: null` must not 422.
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "assistant", "content": null},
                {"role": "user", "content": "ping"}
            ]
        });
        let (status, value) = post_raw(
            echo_state(None),
            &body.to_string(),
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            value["choices"][0]["message"]["content"].as_str(),
            Some("echo: ping")
        );
    }

    #[tokio::test]
    async fn malformed_json_is_400_envelope() {
        // Bug #2: malformed JSON → 400 with the OpenAI error envelope.
        let (status, value) = post_raw(
            echo_state(None),
            "{not valid json",
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error_envelope(&value, "invalid_request_error");
    }

    #[tokio::test]
    async fn missing_messages_is_400_envelope() {
        // Bug #2: a missing required field must be 400 JSON, not a plain 422.
        let (status, value) = post_raw(
            echo_state(None),
            r#"{"model":"auto"}"#,
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error_envelope(&value, "invalid_request_error");
    }

    #[tokio::test]
    async fn missing_content_type_is_400_envelope() {
        // Bug #2: a missing Content-Type must be 400 JSON, not a plain 415.
        let body = serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "ping"}]
        });
        let (status, value) = post_raw(echo_state(None), &body.to_string(), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_error_envelope(&value, "invalid_request_error");
    }

    #[tokio::test]
    async fn unknown_model_is_404_model_not_found() {
        // Bug #3: an unknown model is a permanent client error → non-retryable
        // 404 model_not_found, never a retryable 502.
        let body = serde_json::json!({
            "model": "does-not-exist",
            "messages": [{"role": "user", "content": "ping"}]
        });
        let (status, value) = post_raw(
            echo_state_no_default(),
            &body.to_string(),
            Some("application/json"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_error_envelope(&value, "invalid_request_error");
        assert_eq!(value["error"]["code"].as_str(), Some("model_not_found"));
    }

    fn detected(name: &str, cost_class: CostClass) -> DetectedTool {
        DetectedTool {
            name: name.to_string(),
            kind: ToolKind::CodingAgent,
            installed: true,
            path: None,
            version: None,
            invocation_modes: vec![InvocationMode::Api],
            cli_invocation: None,
            cost_class,
            capabilities: vec![Capability::Code],
            config_path: None,
            models: Vec::new(),
            server_running: None,
            enabled: true,
        }
    }
}
