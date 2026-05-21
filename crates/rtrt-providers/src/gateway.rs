//! Multi-provider gateway + per-request metrics.
//!
//! Sits in front of an arbitrary number of [`Provider`] backends and dispatches
//! [`ChatRequest`]s by model id. Records latency, token usage, and outcome for
//! every call so dashboards / observability surfaces can read live counters.
//!
//! Routing model: a provider is registered with a name (`"anthropic"`,
//! `"openai"`, `"ollama"`, …) and a set of model-id prefixes it owns. The
//! first registered prefix that matches wins; unmatched models fall back to a
//! default provider if one is configured.
//!
//! Metrics are kept in-memory only at v0.3 first cut. A trailing-window cap
//! prevents unbounded growth — old metrics roll off the back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rtrt_core::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::{ChatRequest, ChatResponse, Provider, Usage};

/// One observation per chat call. Cheap to clone (`Arc`-free, all owned data).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMetric {
    /// Provider name as registered with the gateway.
    pub provider: String,
    /// Model id the caller asked for.
    pub model: String,
    /// Unix seconds at the start of the call.
    pub started_at: u64,
    /// Wall-clock latency.
    pub latency_ms: u64,
    /// Token usage. Zero when the call failed before a usage block was returned.
    pub usage: Usage,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Truncated error message when `!ok`.
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct GatewaySummary {
    pub calls: u64,
    pub successes: u64,
    pub failures: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_latency_ms: u64,
}

impl GatewaySummary {
    pub fn record(&mut self, m: &RequestMetric) {
        self.calls += 1;
        if m.ok {
            self.successes += 1;
        } else {
            self.failures += 1;
        }
        self.total_input_tokens += m.usage.input_tokens;
        self.total_output_tokens += m.usage.output_tokens;
        self.total_latency_ms += m.latency_ms;
    }

    pub fn avg_latency_ms(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total_latency_ms as f64 / self.calls as f64
        }
    }
}

struct Registration {
    name: String,
    prefixes: Vec<String>,
    provider: Box<dyn Provider>,
}

pub struct Gateway {
    providers: Vec<Registration>,
    default: Option<usize>,
    metrics: Arc<Mutex<MetricsBuffer>>,
    /// Cap on retained per-request metrics. Older entries roll off.
    pub metric_window: usize,
}

pub struct MetricsBuffer {
    inner: std::collections::VecDeque<RequestMetric>,
    summary: GatewaySummary,
    by_provider: HashMap<String, GatewaySummary>,
}

impl Default for Gateway {
    fn default() -> Self {
        Self::new()
    }
}

impl Gateway {
    /// Constructs a gateway from environment variables. The intent is "spin up
    /// a usable gateway in one line" — primarily for the dashboard binary.
    ///
    /// Honours:
    /// - `ANTHROPIC_API_KEY` — registers an Anthropic provider for `claude-*`.
    /// - `OPENAI_API_KEY` — registers an OpenAI provider for `gpt-*` / `o*`.
    /// - `RTRT_OPENAI_COMPAT_URL` (+ optional `RTRT_OPENAI_COMPAT_API_KEY`) —
    ///   registers an OpenAI-compatible provider as the default fallback.
    pub fn from_env() -> Self {
        let mut gw = Gateway::new();
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            gw = gw.register(
                "anthropic",
                Box::new(crate::AnthropicProvider::new(key)),
                ["claude-"],
            );
        }
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            gw = gw.register(
                "openai",
                Box::new(crate::OpenAIProvider::new(key)),
                ["gpt-", "o1-", "o3-", "o4-"],
            );
        }
        if let Ok(url) = std::env::var("RTRT_OPENAI_COMPAT_URL") {
            let mut p = crate::OpenAICompatibleProvider::new("openai-compat", url);
            if let Ok(key) = std::env::var("RTRT_OPENAI_COMPAT_API_KEY") {
                p = p.with_api_key(key);
            }
            gw = gw.register("openai-compat", Box::new(p), [] as [&'static str; 0]);
            gw = gw.with_default_last();
        }
        gw
    }

    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
            default: None,
            metrics: Arc::new(Mutex::new(MetricsBuffer {
                inner: std::collections::VecDeque::new(),
                summary: GatewaySummary::default(),
                by_provider: HashMap::new(),
            })),
            metric_window: 1024,
        }
    }

    /// Register a provider under `name`, owning any model id starting with one
    /// of `model_prefixes`. Returns `self` for chaining.
    pub fn register(
        mut self,
        name: impl Into<String>,
        provider: Box<dyn Provider>,
        model_prefixes: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        self.providers.push(Registration {
            name: name.into(),
            prefixes: model_prefixes.into_iter().map(|s| s.to_string()).collect(),
            provider,
        });
        self
    }

    /// Mark the most-recently-registered provider as the fallback for models
    /// that don't match any prefix.
    pub fn with_default_last(mut self) -> Self {
        self.default = self.providers.len().checked_sub(1);
        self
    }

    /// Returns a handle into the metrics buffer. Cheap to clone; lock briefly
    /// when reading.
    pub fn metrics(&self) -> Arc<Mutex<MetricsBuffer>> {
        self.metrics.clone()
    }

    fn lookup(&self, model: &str) -> Option<(&str, &dyn Provider)> {
        for r in &self.providers {
            if r.prefixes.iter().any(|p| model.starts_with(p.as_str())) {
                return Some((&r.name, r.provider.as_ref()));
            }
        }
        if let Some(i) = self.default {
            let r = &self.providers[i];
            return Some((&r.name, r.provider.as_ref()));
        }
        None
    }

    /// Dispatches the chat request to the matching provider and records a
    /// metric. The caller sees the [`ChatResponse`]; the metric is observable
    /// via [`Gateway::metrics`].
    pub async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let Some((name, provider)) = self.lookup(&req.model) else {
            return Err(Error::Provider(format!(
                "gateway: no provider registered for model '{}'",
                req.model
            )));
        };
        let started = Instant::now();
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let model = req.model.clone();
        let result = provider.chat(req).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let metric = match &result {
            Ok(resp) => RequestMetric {
                provider: name.to_string(),
                model: model.clone(),
                started_at,
                latency_ms,
                usage: resp.usage,
                ok: true,
                error: None,
            },
            Err(e) => RequestMetric {
                provider: name.to_string(),
                model: model.clone(),
                started_at,
                latency_ms,
                usage: Usage::default(),
                ok: false,
                error: Some(truncate(&format!("{e}"), 256)),
            },
        };
        self.push_metric(metric);
        result
    }

    fn push_metric(&self, m: RequestMetric) {
        let mut buf = match self.metrics.lock() {
            Ok(b) => b,
            Err(p) => p.into_inner(),
        };
        buf.summary.record(&m);
        buf.by_provider
            .entry(m.provider.clone())
            .or_default()
            .record(&m);
        buf.inner.push_back(m);
        while buf.inner.len() > self.metric_window {
            buf.inner.pop_front();
        }
    }
}

impl MetricsBuffer {
    pub fn summary(&self) -> GatewaySummary {
        self.summary
    }

    pub fn by_provider(&self) -> &HashMap<String, GatewaySummary> {
        &self.by_provider
    }

    pub fn recent(&self, limit: usize) -> Vec<RequestMetric> {
        self.inner.iter().rev().take(limit).cloned().collect()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Convenience: read-only view of a [`MetricsBuffer`] held inside [`Gateway::metrics`].
/// Use this in dashboards / MCP tools to avoid teaching every caller about
/// the internal lock shape.
pub struct MetricsView<'a> {
    buffer: &'a MetricsBuffer,
}

impl<'a> MetricsView<'a> {
    pub fn new(buffer: &'a MetricsBuffer) -> Self {
        Self { buffer }
    }
    pub fn summary(&self) -> GatewaySummary {
        self.buffer.summary()
    }
    pub fn by_provider(&self) -> &HashMap<String, GatewaySummary> {
        self.buffer.by_provider()
    }
    pub fn recent(&self, limit: usize) -> Vec<RequestMetric> {
        self.buffer.recent(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, ChatResponse, Role};
    use async_trait::async_trait;

    struct Echo {
        name: &'static str,
        tokens: (u64, u64),
    }

    #[async_trait]
    impl Provider for Echo {
        fn name(&self) -> &str {
            self.name
        }
        fn supported_models(&self) -> &[&'static str] {
            &[]
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse {
                provider: self.name.to_string(),
                model: req.model,
                content: format!("hello from {}", self.name),
                usage: Usage {
                    input_tokens: self.tokens.0,
                    output_tokens: self.tokens.1,
                    ..Default::default()
                },
            })
        }
    }

    fn req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: None,
            temperature: None,
        }
    }

    #[tokio::test]
    async fn dispatches_by_prefix() {
        let gw = Gateway::new()
            .register(
                "anthropic",
                Box::new(Echo {
                    name: "anthropic",
                    tokens: (1, 2),
                }),
                ["claude-"],
            )
            .register(
                "openai",
                Box::new(Echo {
                    name: "openai",
                    tokens: (3, 4),
                }),
                ["gpt-", "o"],
            );
        let claude = gw.chat(req("claude-haiku-4-5")).await.unwrap();
        assert_eq!(claude.provider, "anthropic");
        let gpt = gw.chat(req("gpt-5.4-mini")).await.unwrap();
        assert_eq!(gpt.provider, "openai");
    }

    #[tokio::test]
    async fn records_metrics() {
        let gw = Gateway::new().register(
            "e",
            Box::new(Echo {
                name: "e",
                tokens: (10, 20),
            }),
            ["x-"],
        );
        gw.chat(req("x-small")).await.unwrap();
        gw.chat(req("x-medium")).await.unwrap();
        let metrics = gw.metrics();
        let guard = metrics.lock().unwrap();
        let view = MetricsView::new(&guard);
        let s = view.summary();
        assert_eq!(s.calls, 2);
        assert_eq!(s.successes, 2);
        assert_eq!(s.total_input_tokens, 20);
        assert_eq!(s.total_output_tokens, 40);
        assert_eq!(view.recent(1).len(), 1);
    }

    #[tokio::test]
    async fn unknown_model_errors_without_default() {
        let gw = Gateway::new().register(
            "e",
            Box::new(Echo {
                name: "e",
                tokens: (0, 0),
            }),
            ["claude-"],
        );
        let err = gw.chat(req("gpt-x")).await.unwrap_err();
        assert!(format!("{err}").contains("no provider registered"), "{err}");
    }

    #[tokio::test]
    async fn default_provider_falls_through() {
        let gw = Gateway::new()
            .register(
                "e",
                Box::new(Echo {
                    name: "e",
                    tokens: (0, 0),
                }),
                ["claude-"],
            )
            .register(
                "fallback",
                Box::new(Echo {
                    name: "fallback",
                    tokens: (0, 0),
                }),
                [],
            )
            .with_default_last();
        let r = gw.chat(req("anything")).await.unwrap();
        assert_eq!(r.provider, "fallback");
    }
}
