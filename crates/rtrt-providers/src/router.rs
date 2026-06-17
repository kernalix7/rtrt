use std::cmp::Ordering;

use rtrt_core::{Capability, CostClass, DetectedTool, Error, InvocationMode, Result, ToolKind};
use serde::{Deserialize, Serialize};

use crate::{Mode, usage::UsageSnapshot};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Prefer {
    #[default]
    Cheapest,
    Quality,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRequest {
    pub capability: Option<Capability>,
    pub prefer: Prefer,
    pub target: Option<String>,
    pub model: Option<String>,
    pub mode: Option<Mode>,
}

impl Default for RouteRequest {
    fn default() -> Self {
        Self {
            capability: None,
            prefer: Prefer::Cheapest,
            target: None,
            model: None,
            mode: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub target: String,
    pub mode: Mode,
    pub model: Option<String>,
    pub cost_class: CostClass,
    pub reason: String,
    pub alternatives: Vec<RouteAlternative>,
}

/// One target in failover order: the primary pick, then each ranked
/// alternative. `invoke_with_failover` walks these in sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedTarget {
    pub target: String,
    pub mode: Mode,
    pub model: Option<String>,
    pub cost_class: CostClass,
}

impl RouteDecision {
    /// The full failover walk: the chosen target first, then every ranked
    /// alternative in order. The router has already demoted exhausted targets to
    /// the tail, so walking this list respects the local-free → subscription →
    /// metered preference among healthy targets and only reaches exhausted ones
    /// as a last resort.
    pub fn ranked_targets(&self) -> Vec<RankedTarget> {
        let mut targets = Vec::with_capacity(1 + self.alternatives.len());
        targets.push(RankedTarget {
            target: self.target.clone(),
            mode: self.mode,
            model: self.model.clone(),
            cost_class: self.cost_class,
        });
        for alt in &self.alternatives {
            targets.push(RankedTarget {
                target: alt.target.clone(),
                mode: alt.mode,
                model: alt.model.clone(),
                cost_class: alt.cost_class,
            });
        }
        targets
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteAlternative {
    pub target: String,
    pub mode: Mode,
    pub model: Option<String>,
    pub cost_class: CostClass,
    pub capabilities: Vec<Capability>,
    pub headroom: String,
    pub reason: String,
}

/// A candidate is `Near` its cap once its scarcest dimension dips below this
/// fraction of the configured limit. Below this we shift traffic to roomier
/// targets even within the same cost tier (load-balance away from soon-to-be
/// throttled providers). It is a ratio, not a flat behavioural cap: it scales
/// with whatever `[limits]` the user configured.
const NEAR_LIMIT_FRACTION: f64 = 0.15;

#[derive(Debug, Clone)]
struct Candidate<'a> {
    tool: &'a DetectedTool,
    mode: Mode,
    model: Option<String>,
    capability_fit: usize,
    headroom: HeadroomScore,
    health: HeadroomHealth,
}

/// Where a candidate sits relative to its configured `[limits]` cap.
///
/// Only meaningful when a cap exists: targets with no cap are always `Healthy`
/// (there is nothing to balance against, so they keep pure cost-tier ordering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadroomHealth {
    /// No cap configured, or comfortably within the cap.
    Healthy,
    /// A cap is configured and the scarcest dimension is under
    /// [`NEAR_LIMIT_FRACTION`] of it — penalized within its cost tier.
    Near,
    /// A cap is configured and a dimension is fully spent
    /// (remaining tokens or requests == 0) — demoted below every other
    /// candidate so it is only ever a last-resort fallback.
    Exhausted,
}

impl HeadroomHealth {
    /// Exhausted candidates sink to the very bottom of the ranking regardless of
    /// cost class; this flag is the first sort key.
    fn exhausted_rank(self) -> u8 {
        u8::from(matches!(self, Self::Exhausted))
    }

    /// Within a cost tier, `Near` candidates are penalized so roomier targets
    /// win the tie; `Healthy`/`Exhausted` carry no extra in-tier penalty
    /// (exhausted is already handled by [`Self::exhausted_rank`]).
    fn near_rank(self) -> u8 {
        u8::from(matches!(self, Self::Near))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadroomScore {
    Known {
        tokens: Option<HeadroomDimension>,
        requests: Option<HeadroomDimension>,
    },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadroomDimension {
    remaining: u64,
    limit: u64,
}

impl HeadroomScore {
    fn from_usage(target: &str, usage: &UsageSnapshot) -> Self {
        usage
            .headroom(target)
            .map(|quota| {
                let tokens = quota.token_limit_configured.then_some(HeadroomDimension {
                    remaining: quota.remaining,
                    limit: quota.limit,
                });
                let requests = quota
                    .request_limit
                    .zip(quota.request_remaining)
                    .map(|(limit, remaining)| HeadroomDimension { remaining, limit });
                Self::Known { tokens, requests }
            })
            .unwrap_or(Self::Unknown)
    }

    fn label(self) -> String {
        match self {
            Self::Known { tokens, requests } => {
                let mut parts = Vec::new();
                if let Some(tokens) = tokens {
                    parts.push(format!(
                        "{}/{} tokens remaining ({:.1}%)",
                        tokens.remaining,
                        tokens.limit,
                        tokens.remaining_percent()
                    ));
                }
                if let Some(requests) = requests {
                    parts.push(format!(
                        "{}/{} requests remaining ({:.1}%)",
                        requests.remaining,
                        requests.limit,
                        requests.remaining_percent()
                    ));
                }
                if parts.is_empty() {
                    "unknown".to_string()
                } else {
                    parts.join(", ")
                }
            }
            Self::Unknown => "unknown".to_string(),
        }
    }

    /// Classify this candidate against its configured cap. `Unknown` (no cap)
    /// and capped-but-comfortable both report `Healthy`; a spent dimension is
    /// `Exhausted`; a dimension under [`NEAR_LIMIT_FRACTION`] is `Near`.
    fn health(self, near_fraction: f64) -> HeadroomHealth {
        let Self::Known { tokens, requests } = self else {
            return HeadroomHealth::Healthy;
        };
        let dims = [tokens, requests].into_iter().flatten();
        let mut worst = HeadroomHealth::Healthy;
        for dim in dims {
            let dim_health = if dim.remaining == 0 {
                HeadroomHealth::Exhausted
            } else if dim.remaining_percent() < near_fraction * 100.0 {
                HeadroomHealth::Near
            } else {
                HeadroomHealth::Healthy
            };
            // Exhausted dominates Near dominates Healthy; the scarcest dimension
            // decides the candidate's health.
            worst = match (worst, dim_health) {
                (HeadroomHealth::Exhausted, _) | (_, HeadroomHealth::Exhausted) => {
                    HeadroomHealth::Exhausted
                }
                (HeadroomHealth::Near, _) | (_, HeadroomHealth::Near) => HeadroomHealth::Near,
                _ => HeadroomHealth::Healthy,
            };
        }
        worst
    }

    fn limiting_dimension(self) -> Option<HeadroomDimension> {
        match self {
            Self::Known { tokens, requests } => match (tokens, requests) {
                (Some(tokens), Some(requests)) => {
                    Some(if tokens.remaining_fraction_cmp(requests).is_lt() {
                        tokens
                    } else {
                        requests
                    })
                }
                (Some(tokens), None) => Some(tokens),
                (None, Some(requests)) => Some(requests),
                (None, None) => None,
            },
            Self::Unknown => None,
        }
    }
}

impl HeadroomDimension {
    fn remaining_percent(self) -> f64 {
        if self.limit == 0 {
            return 0.0;
        }
        self.remaining as f64 / self.limit as f64 * 100.0
    }

    fn remaining_fraction_cmp(self, other: Self) -> Ordering {
        if self.limit == 0 || other.limit == 0 {
            return match (self.limit == 0, other.limit == 0) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => Ordering::Equal,
            };
        }
        (self.remaining as u128 * other.limit as u128)
            .cmp(&(other.remaining as u128 * self.limit as u128))
    }
}

pub fn select_route(
    req: &RouteRequest,
    tools: &[DetectedTool],
    usage: &UsageSnapshot,
) -> Result<RouteDecision> {
    if let Some(target) = req.target.as_deref() {
        return explicit_route(req, tools, usage, target);
    }

    let mut candidates = tools
        .iter()
        .filter(|tool| tool.installed && tool.enabled)
        .filter(|tool| {
            req.capability
                .is_none_or(|capability| tool.capabilities.contains(&capability))
        })
        .filter_map(|tool| candidate_for(req, usage, tool).ok())
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Err(Error::Provider(format!(
            "route: no installed and enabled target{}",
            capability_suffix(req.capability)
        )));
    }

    candidates.sort_by(|left, right| compare_candidates(req.prefer, left, right));
    let chosen = candidates.remove(0);
    Ok(decision_from_candidate(req, chosen, candidates))
}

fn explicit_route(
    req: &RouteRequest,
    tools: &[DetectedTool],
    usage: &UsageSnapshot,
    target: &str,
) -> Result<RouteDecision> {
    let normalized = target.to_ascii_lowercase();
    let tool = tools
        .iter()
        .find(|tool| tool.name == target || tool.name == normalized)
        .ok_or_else(|| Error::Provider(format!("route: target '{target}' was not detected")))?;
    if !tool.installed {
        return Err(Error::Provider(format!(
            "route: target '{}' is not installed",
            tool.name
        )));
    }
    if !tool.enabled {
        return Err(Error::Provider(format!(
            "route: target '{}' is disabled",
            tool.name
        )));
    }
    if let Some(capability) = req.capability {
        if !tool.capabilities.contains(&capability) {
            return Err(Error::Provider(format!(
                "route: target '{}' does not provide {:?}",
                tool.name, capability
            )));
        }
    }
    let candidate = candidate_for(req, usage, tool)?;
    let reason = format!(
        "explicit target '{}' selected; mode={} cost={} headroom={}",
        candidate.tool.name,
        mode_label(candidate.mode),
        cost_class_label(candidate.tool.cost_class),
        candidate.headroom.label()
    );
    Ok(RouteDecision {
        target: candidate.tool.name.clone(),
        mode: candidate.mode,
        model: candidate.model,
        cost_class: candidate.tool.cost_class,
        reason,
        alternatives: Vec::new(),
    })
}

fn decision_from_candidate(
    req: &RouteRequest,
    chosen: Candidate<'_>,
    alternatives: Vec<Candidate<'_>>,
) -> RouteDecision {
    let reason = route_reason(req, &chosen);
    RouteDecision {
        target: chosen.tool.name.clone(),
        mode: chosen.mode,
        model: chosen.model.clone(),
        cost_class: chosen.tool.cost_class,
        reason,
        alternatives: alternatives
            .into_iter()
            .map(|candidate| RouteAlternative {
                target: candidate.tool.name.clone(),
                mode: candidate.mode,
                model: candidate.model.clone(),
                cost_class: candidate.tool.cost_class,
                capabilities: candidate.tool.capabilities.clone(),
                headroom: candidate.headroom.label(),
                reason: alternative_reason(&candidate),
            })
            .collect(),
    }
}

fn candidate_for<'a>(
    req: &RouteRequest,
    usage: &UsageSnapshot,
    tool: &'a DetectedTool,
) -> Result<Candidate<'a>> {
    let mode = match req.mode.unwrap_or(Mode::Auto) {
        Mode::Auto => auto_mode_for_route(tool).ok_or_else(|| {
            Error::Provider(format!(
                "route: target '{}' has no usable CLI or API invocation",
                tool.name
            ))
        })?,
        Mode::Cli => validate_cli_mode(tool)?,
        Mode::Api => validate_api_mode(tool)?,
    };
    let model = choose_model(req, tool, mode)?;
    let headroom = HeadroomScore::from_usage(&tool.name, usage);
    Ok(Candidate {
        tool,
        mode,
        model,
        capability_fit: capability_fit(req.capability, tool),
        headroom,
        health: headroom.health(NEAR_LIMIT_FRACTION),
    })
}

fn choose_model(req: &RouteRequest, tool: &DetectedTool, mode: Mode) -> Result<Option<String>> {
    if let Some(model) = &req.model {
        return Ok(Some(model.clone()));
    }
    let model = tool.models.first().cloned();
    let cli_requires_model = mode == Mode::Cli
        && tool
            .cli_invocation
            .as_deref()
            .is_some_and(|template| template.contains("{model}"));
    if cli_requires_model && model.is_none() && tool.kind == ToolKind::LocalRuntime {
        return Err(Error::Provider(format!(
            "route: target '{}' needs --model because no installed model was detected",
            tool.name
        )));
    }
    Ok(model)
}

fn validate_cli_mode(tool: &DetectedTool) -> Result<Mode> {
    if tool.invocation_modes.contains(&InvocationMode::Cli) && tool.cli_invocation.is_some() {
        Ok(Mode::Cli)
    } else {
        Err(Error::Provider(format!(
            "route: target '{}' does not support CLI mode",
            tool.name
        )))
    }
}

fn validate_api_mode(tool: &DetectedTool) -> Result<Mode> {
    if tool.invocation_modes.contains(&InvocationMode::Api) {
        Ok(Mode::Api)
    } else {
        Err(Error::Provider(format!(
            "route: target '{}' does not support API mode",
            tool.name
        )))
    }
}

fn auto_mode_for_route(tool: &DetectedTool) -> Option<Mode> {
    if matches!(
        tool.cost_class,
        CostClass::LocalFree | CostClass::SubscriptionFlat
    ) {
        if validate_cli_mode(tool).is_ok() {
            return Some(Mode::Cli);
        }
        if validate_api_mode(tool).is_ok() {
            return Some(Mode::Api);
        }
        return None;
    }
    if validate_cli_mode(tool).is_ok() {
        Some(Mode::Cli)
    } else if validate_api_mode(tool).is_ok() {
        Some(Mode::Api)
    } else {
        None
    }
}

fn compare_candidates(prefer: Prefer, left: &Candidate<'_>, right: &Candidate<'_>) -> Ordering {
    match prefer {
        Prefer::Cheapest | Prefer::Local => compare_cost_first(left, right),
        Prefer::Quality => compare_quality_first(left, right),
    }
}

fn compare_cost_first(left: &Candidate<'_>, right: &Candidate<'_>) -> Ordering {
    // Exhausted targets sink below everything (last-resort only), overriding
    // even cost class. Then the documented cost-tier order. `Near` candidates
    // are penalized WITHIN their cost tier so traffic shifts to roomier peers.
    left.health
        .exhausted_rank()
        .cmp(&right.health.exhausted_rank())
        .then_with(|| cost_rank(left.tool.cost_class).cmp(&cost_rank(right.tool.cost_class)))
        .then_with(|| left.health.near_rank().cmp(&right.health.near_rank()))
        .then_with(|| right.capability_fit.cmp(&left.capability_fit))
        .then_with(|| compare_headroom_desc(left.headroom, right.headroom))
        .then_with(|| left.tool.name.cmp(&right.tool.name))
}

fn compare_quality_first(left: &Candidate<'_>, right: &Candidate<'_>) -> Ordering {
    // Quality-first still demotes exhausted targets to last resort, then ranks
    // by capability, penalizing `Near` candidates before falling back to cost.
    left.health
        .exhausted_rank()
        .cmp(&right.health.exhausted_rank())
        .then_with(|| right.capability_fit.cmp(&left.capability_fit))
        .then_with(|| left.health.near_rank().cmp(&right.health.near_rank()))
        .then_with(|| compare_headroom_desc(left.headroom, right.headroom))
        .then_with(|| cost_rank(left.tool.cost_class).cmp(&cost_rank(right.tool.cost_class)))
        .then_with(|| left.tool.name.cmp(&right.tool.name))
}

fn compare_headroom_desc(left: HeadroomScore, right: HeadroomScore) -> Ordering {
    match (left.limiting_dimension(), right.limiting_dimension()) {
        (Some(left), Some(right)) => right
            .remaining_fraction_cmp(left)
            .then_with(|| right.remaining.cmp(&left.remaining)),
        (Some(HeadroomDimension { remaining: 0, .. }), None) => Ordering::Greater,
        (None, Some(HeadroomDimension { remaining: 0, .. })) => Ordering::Less,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cost_rank(cost_class: CostClass) -> usize {
    // Derived directly from the CostClass ladder: free local work, then
    // already-paid subscriptions, then per-call API spend, then unknown cost.
    match cost_class {
        CostClass::LocalFree => 0,
        CostClass::SubscriptionFlat => 1,
        CostClass::ApiMetered => 2,
        CostClass::Unknown => 3,
    }
}

fn capability_fit(requested: Option<Capability>, tool: &DetectedTool) -> usize {
    // The requested capability contributes one exact-match point; the rest is
    // the declared capability breadth from detection data, with no external
    // weights or fixed score constants.
    usize::from(requested.is_some_and(|capability| tool.capabilities.contains(&capability)))
        + tool.capabilities.len()
}

fn route_reason(req: &RouteRequest, candidate: &Candidate<'_>) -> String {
    let preference = match req.prefer {
        Prefer::Cheapest => "cheapest",
        Prefer::Quality => "quality",
        Prefer::Local => "local",
    };
    let cost_note = if matches!(
        candidate.tool.cost_class,
        CostClass::LocalFree | CostClass::SubscriptionFlat
    ) {
        "no API cost"
    } else {
        "metered cost"
    };
    format!(
        "chose {} ({}, {}) for {preference} routing; mode={} headroom={}{} - {cost_note}",
        candidate.tool.name,
        cost_class_label(candidate.tool.cost_class),
        capability_label(req.capability),
        mode_label(candidate.mode),
        candidate.headroom.label(),
        health_suffix(candidate.health),
    )
}

fn alternative_reason(candidate: &Candidate<'_>) -> String {
    format!(
        "{} candidate; fit={} mode={} headroom={}{}",
        cost_class_label(candidate.tool.cost_class),
        candidate.capability_fit,
        mode_label(candidate.mode),
        candidate.headroom.label(),
        health_suffix(candidate.health),
    )
}

/// A short tag appended to a candidate's reason when its `[limits]` headroom is
/// running out, so `route --explain` shows why a target was penalized/demoted.
fn health_suffix(health: HeadroomHealth) -> &'static str {
    match health {
        HeadroomHealth::Healthy => "",
        HeadroomHealth::Near => " [near-limit: penalized]",
        HeadroomHealth::Exhausted => " [exhausted: demoted to last resort]",
    }
}

fn capability_suffix(capability: Option<Capability>) -> String {
    capability
        .map(|capability| format!(" with {:?} capability", capability))
        .unwrap_or_default()
}

fn capability_label(capability: Option<Capability>) -> &'static str {
    match capability {
        Some(Capability::Reasoning) => "reasoning",
        Some(Capability::Code) => "code",
        Some(Capability::Vision) => "vision",
        Some(Capability::Embed) => "embed",
        Some(Capability::Agentic) => "agentic",
        Some(Capability::CheapBulk) => "cheap-bulk",
        None => "general",
    }
}

fn cost_class_label(cost_class: CostClass) -> &'static str {
    match cost_class {
        CostClass::LocalFree => "local-free",
        CostClass::SubscriptionFlat => "subscription-flat",
        CostClass::ApiMetered => "api-metered",
        CostClass::Unknown => "unknown-cost",
    }
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Cli => "cli",
        Mode::Api => "api",
        Mode::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use rtrt_core::{Capability, InvocationMode};

    use super::*;

    #[test]
    fn cheapest_prefers_local_over_subscription_over_api() {
        let tools = vec![
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
            tool("claude", CostClass::SubscriptionFlat, &[Capability::Code]),
            local_tool("ollama", &[Capability::Code], &["qwen2.5-coder"]),
        ];
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        assert_eq!(decision.target, "ollama");
        assert_eq!(decision.cost_class, CostClass::LocalFree);
    }

    #[test]
    fn capability_filter_works() {
        let tools = vec![
            tool(
                "text-only",
                CostClass::SubscriptionFlat,
                &[Capability::Code],
            ),
            tool("vision-api", CostClass::ApiMetered, &[Capability::Vision]),
        ];
        let req = request(Capability::Vision, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        assert_eq!(decision.target, "vision-api");
    }

    #[test]
    fn explicit_override_wins() {
        let tools = vec![
            local_tool("ollama", &[Capability::Code], &["qwen2.5-coder"]),
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
        ];
        let req = RouteRequest {
            capability: Some(Capability::Code),
            prefer: Prefer::Cheapest,
            target: Some("openai".to_string()),
            model: Some("gpt-test".to_string()),
            mode: Some(Mode::Api),
        };

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        assert_eq!(decision.target, "openai");
        assert_eq!(decision.model.as_deref(), Some("gpt-test"));
        assert!(decision.alternatives.is_empty());
    }

    #[test]
    fn quota_headroom_tie_break() {
        let tools = vec![
            tool("anthropic", CostClass::ApiMetered, &[Capability::Code]),
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
        ];
        let usage = UsageSnapshot::from_usage_and_limits_for_tests(
            [("anthropic", 90), ("openai", 10)],
            [("anthropic", 100), ("openai", 100)],
        );
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &usage).unwrap();

        assert_eq!(decision.target, "openai");
    }

    #[test]
    fn request_limit_headroom_tie_break() {
        let tools = vec![
            tool("anthropic", CostClass::ApiMetered, &[Capability::Code]),
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
        ];
        let usage = UsageSnapshot::from_usage_limits_and_requests_for_tests(
            [],
            [],
            [("anthropic", 99), ("openai", 10)],
            [("anthropic", 100), ("openai", 100)],
        );
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &usage).unwrap();

        assert_eq!(decision.target, "openai");
        assert!(decision.reason.contains("90/100 requests remaining"));
    }

    #[test]
    fn exhausted_target_is_demoted_below_cheaper_tiers() {
        // ollama (local-free) is exhausted against its cap; the only roomy
        // option is openai (api-metered). Despite being the cheapest tier,
        // exhausted ollama must lose to the healthy metered target.
        let tools = vec![
            local_tool("ollama", &[Capability::Code], &["qwen2.5-coder"]),
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
        ];
        let usage = UsageSnapshot::from_usage_and_limits_for_tests(
            [("ollama", 100), ("openai", 0)],
            [("ollama", 100), ("openai", 1000)],
        );
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &usage).unwrap();

        assert_eq!(decision.target, "openai");
        // ollama is still present as a last-resort fallback, ranked last.
        let ranked = decision.ranked_targets();
        assert_eq!(ranked.last().unwrap().target, "ollama");
        assert!(
            decision
                .alternatives
                .iter()
                .any(|alt| alt.target == "ollama"
                    && alt.reason.contains("exhausted: demoted to last resort"))
        );
    }

    #[test]
    fn near_limit_target_is_penalized_within_its_cost_tier() {
        // Both are api-metered. anthropic has only 10% of its cap left (near),
        // openai has 90% (healthy). Same tier: the roomier one wins, and
        // anthropic is flagged as penalized.
        let tools = vec![
            tool("anthropic", CostClass::ApiMetered, &[Capability::Code]),
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
        ];
        let usage = UsageSnapshot::from_usage_and_limits_for_tests(
            [("anthropic", 90), ("openai", 10)],
            [("anthropic", 100), ("openai", 100)],
        );
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &usage).unwrap();

        assert_eq!(decision.target, "openai");
        assert!(
            decision.alternatives.iter().any(
                |alt| alt.target == "anthropic" && alt.reason.contains("near-limit: penalized")
            )
        );
    }

    #[test]
    fn no_cap_targets_keep_pure_cost_tier_order() {
        // Neither target has a `[limits]` cap, so there is nothing to balance
        // against: the documented cost-tier order is preserved unchanged.
        let tools = vec![
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
            tool("claude", CostClass::SubscriptionFlat, &[Capability::Code]),
            local_tool("ollama", &[Capability::Code], &["qwen2.5-coder"]),
        ];
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        assert_eq!(decision.target, "ollama");
        assert!(!decision.reason.contains("near-limit"));
        assert!(!decision.reason.contains("exhausted"));
        let ranked = decision.ranked_targets();
        let order = ranked.iter().map(|t| t.target.as_str()).collect::<Vec<_>>();
        assert_eq!(order, vec!["ollama", "claude", "openai"]);
    }

    #[test]
    fn request_exhaustion_also_demotes_even_with_token_room() {
        // openai still has plenty of token budget but has spent every request:
        // the scarcest dimension (requests) drives exhaustion, so it is demoted.
        let tools = vec![
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
            tool("anthropic", CostClass::ApiMetered, &[Capability::Code]),
        ];
        let usage = UsageSnapshot::from_usage_limits_and_requests_for_tests(
            [("openai", 10), ("anthropic", 10)],
            [("openai", 1000), ("anthropic", 1000)],
            [("openai", 50), ("anthropic", 1)],
            [("openai", 50), ("anthropic", 50)],
        );
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &usage).unwrap();

        assert_eq!(decision.target, "anthropic");
        assert_eq!(decision.ranked_targets().last().unwrap().target, "openai");
    }

    fn request(capability: Capability, prefer: Prefer) -> RouteRequest {
        RouteRequest {
            capability: Some(capability),
            prefer,
            target: None,
            model: None,
            mode: None,
        }
    }

    fn local_tool(name: &str, capabilities: &[Capability], models: &[&str]) -> DetectedTool {
        let mut detected = tool(name, CostClass::LocalFree, capabilities);
        detected.models = models.iter().map(|model| (*model).to_string()).collect();
        detected.cli_invocation = Some("ollama run {model} {prompt}".to_string());
        detected.kind = ToolKind::LocalRuntime;
        detected
    }

    fn tool(name: &str, cost_class: CostClass, capabilities: &[Capability]) -> DetectedTool {
        let (invocation_modes, cli_invocation) = match cost_class {
            CostClass::ApiMetered => (vec![InvocationMode::Api], None),
            _ => (
                vec![InvocationMode::Cli],
                Some(format!("{name} {{prompt}}")),
            ),
        };
        DetectedTool {
            name: name.to_string(),
            kind: ToolKind::CodingAgent,
            installed: true,
            path: None,
            version: None,
            invocation_modes,
            cli_invocation,
            cost_class,
            capabilities: capabilities.to_vec(),
            config_path: None,
            models: Vec::new(),
            server_running: None,
            enabled: true,
        }
    }
}
