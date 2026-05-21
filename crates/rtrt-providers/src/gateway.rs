//! Multi-provider gateway + per-request metrics + optional budget meter.
//!
//! Sits in front of an arbitrary number of [`Provider`] backends and dispatches
//! [`ChatRequest`]s by model id. Records latency, token usage, and outcome for
//! every call so dashboards / observability surfaces can read live counters
//! and stitch multi-turn flows via [`RequestMetric::parent_id`].
//!
//! Routing model: a provider is registered with a name (`"anthropic"`,
//! `"openai"`, `"ollama"`, …) and a set of model-id prefixes it owns. The
//! first registered prefix that matches wins; unmatched models fall back to a
//! default provider if one is configured.
//!
//! Metrics are kept in-memory only at first cut. A trailing-window cap
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
    /// Monotonic per-gateway counter assigned in record order. Stable for the
    /// lifetime of the gateway; downstream traces use this as the trace id.
    pub id: u64,
    /// Optional parent trace id for multi-turn flows. Set via
    /// [`Gateway::chat_with_parent`].
    #[serde(default)]
    pub parent_id: Option<u64>,
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
    /// Estimated USD cost based on the gateway's pricing table. `0.0` when no
    /// budget is attached or the model has no pricing entry.
    #[serde(default)]
    pub cost_usd: f64,
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
    #[serde(default)]
    pub total_cost_usd: f64,
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
        self.total_cost_usd += m.cost_usd;
    }

    pub fn avg_latency_ms(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.total_latency_ms as f64 / self.calls as f64
        }
    }
}

/// USD pricing per million tokens for a model. Used by [`Budget`] to estimate
/// per-call cost.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
}

/// Snapshot list-prices for common Anthropic and OpenAI models. Defaults for
/// [`Budget::new`]; override per model id when they shift upstream.
pub fn default_pricing() -> HashMap<String, ModelPricing> {
    let mut m = HashMap::new();
    m.insert(
        "claude-opus-4-7".into(),
        ModelPricing {
            input_usd_per_mtok: 15.0,
            output_usd_per_mtok: 75.0,
        },
    );
    m.insert(
        "claude-sonnet-4-6".into(),
        ModelPricing {
            input_usd_per_mtok: 3.0,
            output_usd_per_mtok: 15.0,
        },
    );
    m.insert(
        "claude-haiku-4-5".into(),
        ModelPricing {
            input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 5.0,
        },
    );
    m.insert(
        "gpt-5.4".into(),
        ModelPricing {
            input_usd_per_mtok: 5.0,
            output_usd_per_mtok: 15.0,
        },
    );
    m.insert(
        "gpt-5.4-mini".into(),
        ModelPricing {
            input_usd_per_mtok: 0.6,
            output_usd_per_mtok: 2.4,
        },
    );
    m.insert(
        "gpt-5.3-codex-spark".into(),
        ModelPricing {
            input_usd_per_mtok: 0.5,
            output_usd_per_mtok: 2.0,
        },
    );
    m
}

/// Optional per-gateway spending limit. When attached, every chat call is
/// priced against the model's [`ModelPricing`] entry and added to a running
/// total. Once the total exceeds `cap_usd`, subsequent calls are rejected
/// with `Error::Provider("gateway: budget exceeded …")` before they reach the
/// upstream provider. Pricing for unknown models is treated as zero so the
/// budget never blocks a local Ollama call by accident.
#[derive(Debug, Clone)]
pub struct Budget {
    pub cap_usd: f64,
    pub pricing: HashMap<String, ModelPricing>,
}

impl Budget {
    pub fn new(cap_usd: f64) -> Self {
        Self {
            cap_usd,
            pricing: default_pricing(),
        }
    }
    pub fn with_pricing(mut self, pricing: HashMap<String, ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }
    pub fn upsert_model(mut self, model: impl Into<String>, p: ModelPricing) -> Self {
        self.pricing.insert(model.into(), p);
        self
    }
    /// USD cost estimate for a (model, usage) pair. Returns `0.0` when the
    /// model has no pricing entry.
    pub fn cost_for(&self, model: &str, usage: &Usage) -> f64 {
        let p = match self.pricing.get(model) {
            Some(p) => *p,
            None => return 0.0,
        };
        let inp = usage.input_tokens as f64 / 1_000_000.0 * p.input_usd_per_mtok;
        let out = usage.output_tokens as f64 / 1_000_000.0 * p.output_usd_per_mtok;
        inp + out
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
    budget: Option<Arc<Budget>>,
    next_metric_id: Arc<Mutex<u64>>,
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
            budget: None,
            next_metric_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Attaches a spending limit. Once cumulative cost exceeds the cap,
    /// subsequent `chat` calls return an error before reaching the provider.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = Some(Arc::new(budget));
        self
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
        self.chat_inner(req, None).await
    }

    /// Per-call variant of [`chat`] that records a parent trace id, letting
    /// callers stitch multi-turn flows into a single observable trace.
    pub async fn chat_with_parent(
        &self,
        req: ChatRequest,
        parent_id: Option<u64>,
    ) -> Result<ChatResponse> {
        self.chat_inner(req, parent_id).await
    }

    async fn chat_inner(&self, req: ChatRequest, parent_id: Option<u64>) -> Result<ChatResponse> {
        // Budget gate: refuse before dispatch when over cap.
        if let Some(b) = &self.budget {
            let spent = self
                .metrics
                .lock()
                .ok()
                .map(|g| g.summary.total_cost_usd)
                .unwrap_or(0.0);
            if spent >= b.cap_usd {
                return Err(Error::Provider(format!(
                    "gateway: budget exceeded ({:.4} ≥ cap {:.4} USD)",
                    spent, b.cap_usd
                )));
            }
        }
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
        let id = {
            let mut guard = self
                .next_metric_id
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let id = *guard;
            *guard = guard.saturating_add(1);
            id
        };
        let (usage, ok, err) = match &result {
            Ok(resp) => (resp.usage, true, None),
            Err(e) => (
                Usage::default(),
                false,
                Some(truncate(&format!("{e}"), 256)),
            ),
        };
        let cost = self
            .budget
            .as_ref()
            .map(|b| b.cost_for(&model, &usage))
            .unwrap_or(0.0);
        let metric = RequestMetric {
            id,
            parent_id,
            provider: name.to_string(),
            model: model.clone(),
            started_at,
            latency_ms,
            usage,
            cost_usd: cost,
            ok,
            error: err,
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
    async fn budget_blocks_when_exceeded() {
        let pricing = {
            let mut m = HashMap::new();
            m.insert(
                "x-expensive".to_string(),
                ModelPricing {
                    input_usd_per_mtok: 1_000_000.0,
                    output_usd_per_mtok: 1_000_000.0,
                },
            );
            m
        };
        let gw = Gateway::new()
            .register(
                "e",
                Box::new(Echo {
                    name: "e",
                    tokens: (1, 1),
                }),
                ["x-"],
            )
            .with_budget(Budget::new(0.5).with_pricing(pricing));
        // First call costs 2 USD, pushing the running total past the 0.5 cap.
        gw.chat(req("x-expensive")).await.unwrap();
        let err = gw.chat(req("x-expensive")).await.unwrap_err();
        assert!(format!("{err}").contains("budget exceeded"), "{err}");
    }

    #[tokio::test]
    async fn chat_with_parent_records_link() {
        let gw = Gateway::new().register(
            "e",
            Box::new(Echo {
                name: "e",
                tokens: (1, 1),
            }),
            ["x-"],
        );
        gw.chat(req("x-a")).await.unwrap();
        gw.chat_with_parent(req("x-b"), Some(1)).await.unwrap();
        let metrics = gw.metrics();
        let guard = metrics.lock().unwrap();
        let recent = guard.recent(2);
        // recent() returns newest-first; latest call should have parent_id = Some(1)
        assert_eq!(recent[0].parent_id, Some(1));
        assert_eq!(recent[1].parent_id, None);
        assert_eq!(recent[0].id, 2);
        assert_eq!(recent[1].id, 1);
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
