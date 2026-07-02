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

use crate::{ChatRequest, ChatResponse, Provider, Role, Usage, usage_ledger};

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

/// Helicone-style retry + fallback policy. Empty default means "single try,
/// no fallback", which is what every Gateway has unless [`Gateway::with_retry`]
/// is called.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Number of attempts on the primary provider (including the first).
    pub max_attempts: u32,
    /// Sleep between retries, in milliseconds. With `backoff_factor == 1.0`
    /// the wait is constant; higher factors apply geometric backoff (the wait
    /// is `backoff_ms * backoff_factor.powi(attempt - 1)`).
    pub backoff_ms: u64,
    /// Multiplier applied to `backoff_ms` per failed attempt. `1.0` keeps the
    /// existing constant-backoff behaviour; `2.0` doubles the wait each retry.
    pub backoff_factor: f32,
    /// When `true` and the primary provider keeps failing, the default
    /// fallback provider (set via [`Gateway::with_default_last`]) gets one
    /// last attempt before the error is surfaced.
    pub fallback_to_default: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff_ms: 0,
            backoff_factor: 1.0,
            fallback_to_default: false,
        }
    }
}

impl RetryPolicy {
    /// Convenience builder for an exponential schedule. `factor < 1.0` is
    /// clamped to 1.0 so the wait never shrinks across retries.
    pub fn exponential(max_attempts: u32, backoff_ms: u64, factor: f32) -> Self {
        Self {
            max_attempts,
            backoff_ms,
            backoff_factor: factor.max(1.0),
            fallback_to_default: false,
        }
    }
}

pub struct Gateway {
    providers: Vec<Registration>,
    default: Option<usize>,
    metrics: Arc<Mutex<MetricsBuffer>>,
    /// Cap on retained per-request metrics. Older entries roll off.
    pub metric_window: usize,
    budget: Option<Arc<Budget>>,
    retry: RetryPolicy,
    next_metric_id: Arc<Mutex<u64>>,
    cache: Option<Arc<Mutex<ResponseCache>>>,
    /// When set, every dispatched request is also appended to the persistent
    /// provider-usage ledger (`usage_ledger::record_invocation`), so gateway
    /// callers (MCP `provider_chat`, dashboard chat/compress/memory daemons)
    /// count toward the same windowed headroom the router balances on.
    /// `Gateway::from_env` enables it; embedded/test gateways built via
    /// `Gateway::new` stay ledger-silent unless opted in.
    record_usage: bool,
}

/// Helicone-style response cache. The key hashes
/// `(model, messages, max_tokens, temperature)` so identical requests reuse
/// the previous response without re-charging the provider. Entries evict
/// FIFO once the configured capacity is reached.
pub(crate) struct ResponseCache {
    cap: usize,
    order: std::collections::VecDeque<u64>,
    entries: HashMap<u64, ChatResponse>,
}

impl ResponseCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            order: std::collections::VecDeque::with_capacity(cap),
            entries: HashMap::with_capacity(cap),
        }
    }

    fn get(&self, key: u64) -> Option<ChatResponse> {
        self.entries.get(&key).cloned()
    }

    fn insert(&mut self, key: u64, resp: ChatResponse) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.entries.len() >= self.cap
            && let Some(victim) = self.order.pop_front()
        {
            self.entries.remove(&victim);
        }
        self.order.push_back(key);
        self.entries.insert(key, resp);
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

fn cache_key(req: &ChatRequest) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    req.model.hash(&mut h);
    for m in &req.messages {
        match m.role {
            Role::System => 0u8,
            Role::User => 1u8,
            Role::Assistant => 2u8,
        }
        .hash(&mut h);
        m.content.hash(&mut h);
    }
    req.max_tokens.hash(&mut h);
    // f32 → bits so the hash is total.
    req.temperature.map(f32::to_bits).hash(&mut h);
    h.finish()
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
    /// Ledger recording is on: from-env gateways serve real provider traffic,
    /// which must count toward routing headroom.
    pub fn from_env() -> Self {
        let mut gw = Gateway::new().with_usage_recording(true);
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
        // Accept either the gateway-specific var or the same
        // `RTRT_PROVIDER_BASE_URL` the `rtrt provider chat` CLI uses, so a
        // single env var configures the local/OpenAI-compatible backend
        // everywhere (CLI, dashboard daemon, SessionEnd compress hook).
        if let Ok(url) = std::env::var("RTRT_OPENAI_COMPAT_URL")
            .or_else(|_| std::env::var("RTRT_PROVIDER_BASE_URL"))
        {
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
            retry: RetryPolicy::default(),
            next_metric_id: Arc::new(Mutex::new(1)),
            cache: None,
            record_usage: false,
        }
    }

    /// Toggle persistent provider-usage ledger recording for every dispatched
    /// request. See the `record_usage` field for the default per constructor.
    pub fn with_usage_recording(mut self, enabled: bool) -> Self {
        self.record_usage = enabled;
        self
    }

    /// Attach a fixed-capacity response cache. When `cap == 0` the cache is
    /// disabled. Cache keys are derived from `(model, messages, max_tokens,
    /// temperature)`.
    pub fn with_cache(mut self, cap: usize) -> Self {
        self.cache = if cap == 0 {
            None
        } else {
            Some(Arc::new(Mutex::new(ResponseCache::new(cap))))
        };
        self
    }

    /// Number of entries currently held in the response cache, or `None` if
    /// no cache is attached.
    pub fn cache_len(&self) -> Option<usize> {
        let c = self.cache.as_ref()?;
        Some(c.lock().map(|g| g.len()).unwrap_or(0))
    }

    /// Attaches a spending limit. Once cumulative cost exceeds the cap,
    /// subsequent `chat` calls return an error before reaching the provider.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = Some(Arc::new(budget));
        self
    }

    /// Attaches a [`RetryPolicy`]. Failed chat attempts on the primary
    /// provider are retried up to `max_attempts` times with constant backoff;
    /// if `fallback_to_default` is set, the default provider (configured via
    /// [`Gateway::with_default_last`]) gets one last attempt afterwards.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
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

    /// Attached budget cap in USD, if any. Used by the dashboard to render the
    /// remaining headroom.
    pub fn budget_cap_usd(&self) -> Option<f64> {
        self.budget.as_ref().map(|b| b.cap_usd)
    }

    /// Cumulative cost of every recorded request, computed against the
    /// attached budget's pricing table. Returns `0.0` when no budget is set.
    pub fn budget_spent_usd(&self) -> f64 {
        let Some(b) = &self.budget else {
            return 0.0;
        };
        let metrics = self.metrics.lock().unwrap_or_else(|p| p.into_inner());
        metrics
            .inner
            .iter()
            .map(|m| b.cost_for(&m.model, &m.usage))
            .sum()
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
        // Cache hits return before retries / metrics so the operator pays
        // neither the latency nor the budget.
        if let Some(cache) = &self.cache {
            let key = cache_key(&req);
            if let Ok(g) = cache.lock()
                && let Some(hit) = g.get(key)
            {
                return Ok(hit);
            }
        }
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
        let Some(primary_idx) = self.lookup_index(&req.model) else {
            return Err(Error::Provider(format!(
                "gateway: no provider registered for model '{}'",
                req.model
            )));
        };
        let attempts = self.retry.max_attempts.max(1);
        let mut last_err: Option<Error> = None;
        let key = cache_key(&req);
        for attempt in 0..attempts {
            if attempt > 0 && self.retry.backoff_ms > 0 {
                let factor = self.retry.backoff_factor.max(1.0);
                let wait = if (factor - 1.0).abs() < f32::EPSILON {
                    self.retry.backoff_ms
                } else {
                    let scale = factor.powi(attempt as i32 - 1) as f64;
                    ((self.retry.backoff_ms as f64) * scale) as u64
                };
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            }
            match self
                .dispatch_once(primary_idx, req.clone(), parent_id)
                .await
            {
                Ok(resp) => {
                    if let Some(cache) = &self.cache
                        && let Ok(mut g) = cache.lock()
                    {
                        g.insert(key, resp.clone());
                    }
                    return Ok(resp);
                }
                Err(e) => last_err = Some(e),
            }
        }
        if self.retry.fallback_to_default
            && let Some(default_idx) = self.default
            && default_idx != primary_idx
        {
            match self.dispatch_once(default_idx, req, parent_id).await {
                Ok(resp) => {
                    if let Some(cache) = &self.cache
                        && let Ok(mut g) = cache.lock()
                    {
                        g.insert(key, resp.clone());
                    }
                    return Ok(resp);
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Provider("gateway: no attempts ran".into())))
    }

    fn lookup_index(&self, model: &str) -> Option<usize> {
        for (i, r) in self.providers.iter().enumerate() {
            if r.prefixes.iter().any(|p| model.starts_with(p.as_str())) {
                return Some(i);
            }
        }
        self.default
    }

    async fn dispatch_once(
        &self,
        idx: usize,
        req: ChatRequest,
        parent_id: Option<u64>,
    ) -> Result<ChatResponse> {
        let registration = &self.providers[idx];
        let name = registration.name.clone();
        let started = Instant::now();
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let model = req.model.clone();
        // Char count of the outbound messages, kept for the estimated ledger
        // row when the provider fails (or reports no usage block).
        let prompt_chars: String = if self.record_usage {
            req.messages.iter().map(|m| m.content.as_str()).collect()
        } else {
            String::new()
        };
        let result = registration.provider.chat(req).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        if self.record_usage {
            record_dispatch_to_ledger(&name, &model, &prompt_chars, &result);
        }
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
            provider: name,
            model,
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

/// Append one dispatched request to the provider-usage ledger, following the
/// ledger convention: real API [`Usage`] counts are recorded exactly with
/// `est = 0`; when no usage is available (failure, or a backend that returns
/// no usage block) tokens are estimated at chars/4 with `est = 1`. Best-effort
/// by contract — `record_invocation` never propagates failures.
fn record_dispatch_to_ledger(
    target: &str,
    model: &str,
    prompt_chars: &str,
    result: &Result<ChatResponse>,
) {
    match result {
        Ok(resp) if resp.usage.total() > 0 => {
            usage_ledger::record_invocation(
                target,
                model,
                resp.usage.input_tokens,
                resp.usage.output_tokens,
                false,
                true,
            );
        }
        Ok(resp) => {
            // Some OpenAI-compatible backends omit the usage block; estimate
            // both sides so the request still counts toward headroom.
            usage_ledger::record_invocation(
                target,
                model,
                usage_ledger::estimate_tokens(prompt_chars),
                usage_ledger::estimate_tokens(&resp.content),
                true,
                true,
            );
        }
        Err(_) => {
            usage_ledger::record_invocation(
                target,
                model,
                usage_ledger::estimate_tokens(prompt_chars),
                0,
                true,
                false,
            );
        }
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
    async fn cache_returns_hit_without_recording_metric() {
        let gw = Gateway::new()
            .register(
                "e",
                Box::new(Echo {
                    name: "e",
                    tokens: (5, 5),
                }),
                ["x-"],
            )
            .with_cache(4);
        gw.chat(req("x-a")).await.unwrap();
        let len_before = gw.cache_len();
        gw.chat(req("x-a")).await.unwrap();
        let len_after = gw.cache_len();
        assert_eq!(len_before, Some(1));
        assert_eq!(len_after, Some(1));
        let metrics = gw.metrics();
        let guard = metrics.lock().unwrap();
        // Only the first call should have recorded a metric — the second was
        // a cache hit and bypassed `dispatch_once`.
        assert_eq!(guard.summary.calls, 1);
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

    /// Always-failing provider for retry tests.
    struct Flaky {
        name: &'static str,
    }

    #[async_trait]
    impl Provider for Flaky {
        fn name(&self) -> &str {
            self.name
        }
        fn supported_models(&self) -> &[&'static str] {
            &[]
        }
        async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse> {
            Err(rtrt_core::Error::Provider("simulated outage".into()))
        }
    }

    #[tokio::test]
    async fn retry_falls_back_to_default_provider() {
        let gw = Gateway::new()
            .register("primary", Box::new(Flaky { name: "primary" }), ["claude-"])
            .register(
                "fallback",
                Box::new(Echo {
                    name: "fallback",
                    tokens: (1, 1),
                }),
                [],
            )
            .with_default_last()
            .with_retry(RetryPolicy {
                max_attempts: 2,
                backoff_ms: 0,
                backoff_factor: 1.0,
                fallback_to_default: true,
            });
        let resp = gw.chat(req("claude-haiku-4-5")).await.unwrap();
        assert_eq!(resp.provider, "fallback");
        let metrics = gw.metrics();
        let g = metrics.lock().unwrap();
        // 2 primary attempts + 1 fallback = 3 records.
        assert_eq!(g.summary.calls, 3);
        assert_eq!(g.summary.failures, 2);
        assert_eq!(g.summary.successes, 1);
    }

    #[tokio::test]
    async fn retry_without_fallback_surfaces_last_error() {
        let gw = Gateway::new()
            .register("primary", Box::new(Flaky { name: "primary" }), ["x-"])
            .with_retry(RetryPolicy {
                max_attempts: 3,
                backoff_ms: 0,
                backoff_factor: 1.0,
                fallback_to_default: false,
            });
        let err = gw.chat(req("x-anything")).await.unwrap_err();
        assert!(format!("{err}").contains("simulated outage"));
        let g = gw.metrics();
        let g = g.lock().unwrap();
        assert_eq!(g.summary.calls, 3);
        assert_eq!(g.summary.failures, 3);
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
