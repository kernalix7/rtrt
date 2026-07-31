use std::cmp::Ordering;

use rtrt_core::{
    Capability, CostClass, DetectedTool, Error, InvocationMode, PoolKey, Result, ToolKind,
    pool_from_model,
};
use serde::{Deserialize, Serialize};

use crate::{
    Mode,
    usage::UsageSnapshot,
    usage_ledger::{PoolCap, RoomBasis},
};

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
    /// Ask for a failover chain even when `target` pins one provider.
    ///
    /// An explicit target normally means "this one, nothing else", so
    /// [`RouteDecision::alternatives`] stays empty and
    /// [`RouteDecision::ranked_targets`] yields a single entry. When the caller
    /// also wants to survive that target failing, this keeps the explicit target
    /// FIRST and appends the remaining ranked candidates behind it — sibling
    /// pools of the same target first, since those honour the explicit choice
    /// most closely.
    ///
    /// Defaults to `false`, and is skipped on the wire in that case, so a
    /// request built or deserialized before this field existed routes exactly as
    /// it always did.
    #[serde(default, skip_serializing_if = "is_false")]
    pub failover: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for RouteRequest {
    fn default() -> Self {
        Self {
            capability: None,
            prefer: Prefer::Cheapest,
            target: None,
            model: None,
            mode: None,
            failover: false,
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

/// Appended to a headroom label when the ceiling behind it belongs to the
/// target rather than the pool, so the number is never read as a per-pool
/// entitlement. The cap is reported whole and never divided between siblings.
const SHARED_CAP_NOTE: &str = " (cap shared with sibling pools)";

#[derive(Debug, Clone)]
struct Candidate<'a> {
    tool: &'a DetectedTool,
    mode: Mode,
    model: Option<String>,
    /// The quota bucket this candidate actually draws from: its target, plus the
    /// upstream pool named by the model prefix when there is one. Two candidates
    /// on the same target but different pools are distinct here, which is what
    /// lets ranking see them as the separate quotas they are.
    pool: PoolKey,
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
        /// The ceiling comes from the target, not from this pool, so every
        /// sibling pool draws the same pot down together. Recorded so the label
        /// can say so; the limit itself is never split.
        shared: bool,
    },
    /// A pooled candidate with no configured ceiling on either axis. The only
    /// signal available is the pool's observed 24h usage, which orders siblings
    /// least-used-first but says nothing about remaining quota — see
    /// [`RoomBasis::ObservedUsage`].
    Observed {
        tokens: u64,
        requests: u64,
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
                Self::Known {
                    tokens,
                    requests,
                    shared: false,
                }
            })
            .unwrap_or(Self::Unknown)
    }

    /// Headroom for the quota bucket a candidate actually draws from.
    ///
    /// An unpooled key falls straight through to the target-level lookup above,
    /// so a deployment with no pools scores byte-identically to before pools
    /// existed. A pooled key reads the pool-level view instead: a
    /// `[limits.<target>.pools.<pool>]` cap charges only that pool, a
    /// target-level cap is reported whole and shared, and no cap at all leaves
    /// only observed usage.
    fn from_pool_usage(key: &PoolKey, usage: &UsageSnapshot) -> Self {
        if !key.is_pooled() {
            return Self::from_usage(key.target_key(), usage);
        }
        match usage.pool_headroom(key) {
            Some(quota) => Self::Known {
                tokens: dimension_from_cap(quota.tokens),
                requests: dimension_from_cap(quota.requests),
                shared: quota.tokens.scope.is_shared() || quota.requests.scope.is_shared(),
            },
            None => {
                let canonical = key.canonical();
                Self::Observed {
                    tokens: usage.usage_by_pool.get(&canonical).copied().unwrap_or(0),
                    requests: usage.requests_by_pool.get(&canonical).copied().unwrap_or(0),
                }
            }
        }
    }

    /// What an ordering involving this candidate was derived from, or `None`
    /// when the candidate carries no cap and no pool identity (nothing to
    /// disclose beyond today's `unknown`).
    fn room_basis(self) -> Option<RoomBasis> {
        match self {
            Self::Known { .. } => Some(RoomBasis::Quota),
            Self::Observed { .. } => Some(RoomBasis::ObservedUsage),
            Self::Unknown => None,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Observed { tokens, requests } => format!(
                "no configured cap; {tokens} tokens / {requests} requests observed in the last 24h"
            ),
            Self::Known {
                tokens,
                requests,
                shared,
            } => {
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
                    return "unknown".to_string();
                }
                let mut label = parts.join(", ");
                if shared {
                    label.push_str(SHARED_CAP_NOTE);
                }
                label
            }
            Self::Unknown => "unknown".to_string(),
        }
    }

    /// Classify this candidate against its configured cap. `Unknown` (no cap)
    /// and capped-but-comfortable both report `Healthy`; a spent dimension is
    /// `Exhausted`; a dimension under [`NEAR_LIMIT_FRACTION`] is `Near`.
    ///
    /// [`Self::Observed`] is `Healthy` for the same reason [`Self::Unknown`] is:
    /// with no configured ceiling there is nothing to be near or out of, and
    /// inventing exhaustion from raw usage would be a fabricated limit.
    fn health(self, near_fraction: f64) -> HeadroomHealth {
        let Self::Known {
            tokens, requests, ..
        } = self
        else {
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
            Self::Known {
                tokens, requests, ..
            } => match (tokens, requests) {
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
            // Observed usage is not a remaining-quota number, so it must never
            // enter the quota-fraction comparison; siblings are separated by
            // `compare_observed_room` instead.
            Self::Observed { .. } | Self::Unknown => None,
        }
    }
}

/// One capped axis of a pool quota, as a headroom dimension. `None` when the
/// axis has no configured ceiling — an unknown limit is left unknown.
fn dimension_from_cap(cap: PoolCap) -> Option<HeadroomDimension> {
    let limit = cap.limit?;
    Some(HeadroomDimension {
        remaining: cap.remaining.unwrap_or_default(),
        limit,
    })
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

    let mut candidates = viable_candidates(req, tools, usage);

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
    let candidate = best_lane_for(req, usage, tool)?;
    // An explicit target normally means "this one, nothing else". Only when the
    // caller asked for failover does the explicit pick grow a tail — and it
    // stays first either way.
    let alternatives = if req.failover {
        failover_alternatives(req, tools, usage, &candidate)
    } else {
        Vec::new()
    };
    let reason = format!(
        "explicit target '{}' selected; mode={} cost={} headroom={}{}{}",
        candidate.tool.name,
        mode_label(candidate.mode),
        cost_class_label(candidate.tool.cost_class),
        candidate.headroom.label(),
        basis_suffix(candidate.headroom),
        failover_suffix(&alternatives),
    );
    Ok(RouteDecision {
        target: candidate.tool.name.clone(),
        mode: candidate.mode,
        model: candidate.model.clone(),
        cost_class: candidate.tool.cost_class,
        reason,
        alternatives: alternatives
            .into_iter()
            .map(route_alternative_from)
            .collect(),
    })
}

/// The candidate an explicit target resolves to.
///
/// An explicit target pins the *target*, not the quota bucket. When that target
/// reaches several upstream pools and the caller named no model, its lanes are
/// ranked and the best one wins, so pinning a target can never strand the
/// request on an exhausted pool while a healthy sibling sits idle. A target with
/// a single lane — every target, until pools are in play — falls through to the
/// original single-candidate path, error messages included.
fn best_lane_for<'a>(
    req: &RouteRequest,
    usage: &UsageSnapshot,
    tool: &'a DetectedTool,
) -> Result<Candidate<'a>> {
    let mut lanes = candidates_for(req, usage, tool);
    if lanes.len() < 2 {
        return candidate_for(req, usage, tool);
    }
    lanes.sort_by(|left, right| compare_candidates(req.prefer, left, right));
    Ok(lanes.remove(0))
}

/// The ranked fallbacks behind an explicit target.
///
/// The explicit target's own bucket is dropped (it is already first), and what
/// remains is ordered sibling pools of that same target first — they honour the
/// explicit choice most closely, differing only in upstream quota — then every
/// other target by the normal ranking. Both groups keep the requested
/// preference, so exhausted buckets still sink to the tail of their group.
fn failover_alternatives<'a>(
    req: &RouteRequest,
    tools: &'a [DetectedTool],
    usage: &UsageSnapshot,
    chosen: &Candidate<'a>,
) -> Vec<Candidate<'a>> {
    // Alternatives are discovered without the explicit target and model pins:
    // those describe the primary pick, not what should stand in for it.
    let open = RouteRequest {
        target: None,
        model: None,
        ..req.clone()
    };
    let (mut siblings, mut others): (Vec<_>, Vec<_>) = viable_candidates(&open, tools, usage)
        .into_iter()
        // Dedupe on the quota bucket, not on the model string: the explicit pick
        // and a re-derived candidate for the same pool are the same lane.
        .filter(|candidate| candidate.pool != chosen.pool)
        .partition(|candidate| candidate.pool.target_key() == chosen.pool.target_key());
    siblings.sort_by(|left, right| compare_candidates(req.prefer, left, right));
    others.sort_by(|left, right| compare_candidates(req.prefer, left, right));
    siblings.into_iter().chain(others).collect()
}

/// Notes the failover tail on the explicit reason. Empty when there is no tail,
/// which keeps the reason byte-identical for the default (no-failover) path.
fn failover_suffix(alternatives: &[Candidate<'_>]) -> String {
    if alternatives.is_empty() {
        return String::new();
    }
    format!(
        "; failover requested, {} ranked fallback(s) behind it",
        alternatives.len()
    )
}

/// Every candidate that could serve this request: one per tool, or one per
/// distinct upstream pool for a tool that reaches several.
fn viable_candidates<'a>(
    req: &RouteRequest,
    tools: &'a [DetectedTool],
    usage: &UsageSnapshot,
) -> Vec<Candidate<'a>> {
    tools
        .iter()
        .filter(|tool| tool.installed && tool.enabled)
        .filter(|tool| {
            req.capability
                .is_none_or(|capability| tool.capabilities.contains(&capability))
        })
        .flat_map(|tool| candidates_for(req, usage, tool))
        .collect()
}

/// One candidate per quota bucket a tool can reach.
///
/// Normally that is exactly one — the tool's first model, as it has always
/// been. A tool whose detected models name two or more distinct upstream pools
/// is really several quotas behind one binary, so it fans out into one candidate
/// per pool and ranking can tell them apart.
fn candidates_for<'a>(
    req: &RouteRequest,
    usage: &UsageSnapshot,
    tool: &'a DetectedTool,
) -> Vec<Candidate<'a>> {
    let lanes = pool_lanes(req, tool);
    if lanes.is_empty() {
        return candidate_for(req, usage, tool).into_iter().collect();
    }
    let Ok(mode) = resolve_mode(req, tool) else {
        return Vec::new();
    };
    lanes
        .into_iter()
        .map(|model| build_candidate(req, usage, tool, mode, Some(model)))
        .collect()
}

/// The tool's models collapsed to one representative per quota bucket, in
/// declared order.
///
/// Empty — meaning "do not fan out" — unless the tool demonstrably reaches two
/// or more *named* pools. One named pool (or none) is a single upstream quota
/// however many model tags it exposes, so those tools keep producing the single
/// candidate they always did. An explicit `model` also pins the bucket, so there
/// is nothing to fan out across.
fn pool_lanes(req: &RouteRequest, tool: &DetectedTool) -> Vec<String> {
    if req.model.is_some() {
        return Vec::new();
    }
    let mut seen: Vec<Option<String>> = Vec::new();
    let mut lanes: Vec<String> = Vec::new();
    for model in &tool.models {
        let pool = pool_from_model(model);
        if seen.contains(&pool) {
            continue;
        }
        seen.push(pool);
        lanes.push(model.clone());
    }
    if seen.iter().flatten().count() < 2 {
        return Vec::new();
    }
    lanes
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
            .map(route_alternative_from)
            .collect(),
    }
}

fn route_alternative_from(candidate: Candidate<'_>) -> RouteAlternative {
    RouteAlternative {
        target: candidate.tool.name.clone(),
        mode: candidate.mode,
        model: candidate.model.clone(),
        cost_class: candidate.tool.cost_class,
        capabilities: candidate.tool.capabilities.clone(),
        headroom: candidate.headroom.label(),
        reason: alternative_reason(&candidate),
    }
}

fn candidate_for<'a>(
    req: &RouteRequest,
    usage: &UsageSnapshot,
    tool: &'a DetectedTool,
) -> Result<Candidate<'a>> {
    let mode = resolve_mode(req, tool)?;
    let model = choose_model(req, tool, mode)?;
    Ok(build_candidate(req, usage, tool, mode, model))
}

fn build_candidate<'a>(
    req: &RouteRequest,
    usage: &UsageSnapshot,
    tool: &'a DetectedTool,
    mode: Mode,
    model: Option<String>,
) -> Candidate<'a> {
    // The bucket comes from the pair actually invoked (target + model), so a
    // model with no pool prefix keeps scoring against the target exactly as it
    // always has.
    let pool = PoolKey::from_target_model(&tool.name, model.as_deref());
    let headroom = HeadroomScore::from_pool_usage(&pool, usage);
    Candidate {
        tool,
        mode,
        model,
        pool,
        capability_fit: capability_fit(req.capability, tool),
        headroom,
        health: headroom.health(NEAR_LIMIT_FRACTION),
    }
}

fn resolve_mode(req: &RouteRequest, tool: &DetectedTool) -> Result<Mode> {
    match req.mode.unwrap_or(Mode::Auto) {
        Mode::Auto => auto_mode_for_route(tool).ok_or_else(|| {
            Error::Provider(format!(
                "route: target '{}' has no usable CLI or API invocation",
                tool.name
            ))
        }),
        Mode::Cli => validate_cli_mode(tool),
        Mode::Api => validate_api_mode(tool),
    }
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
    // Exhausted buckets sink below everything (last-resort only), overriding
    // even cost class. Then the documented cost-tier order. `Near` candidates
    // are penalized WITHIN their cost tier so traffic shifts to roomier peers.
    // Health and headroom are pool-level whenever the candidate names a pool, so
    // an exhausted pool loses to its healthy sibling on the very first key even
    // though both share a target name.
    left.health
        .exhausted_rank()
        .cmp(&right.health.exhausted_rank())
        .then_with(|| cost_rank(left.tool.cost_class).cmp(&cost_rank(right.tool.cost_class)))
        .then_with(|| left.health.near_rank().cmp(&right.health.near_rank()))
        .then_with(|| right.capability_fit.cmp(&left.capability_fit))
        .then_with(|| compare_headroom_desc(left.headroom, right.headroom))
        .then_with(|| compare_observed_room(left.headroom, right.headroom))
        .then_with(|| left.tool.name.cmp(&right.tool.name))
        .then_with(|| left.pool.cmp(&right.pool))
}

fn compare_quality_first(left: &Candidate<'_>, right: &Candidate<'_>) -> Ordering {
    // Quality-first still demotes exhausted buckets to last resort, then ranks
    // by capability, penalizing `Near` candidates before falling back to cost.
    left.health
        .exhausted_rank()
        .cmp(&right.health.exhausted_rank())
        .then_with(|| right.capability_fit.cmp(&left.capability_fit))
        .then_with(|| left.health.near_rank().cmp(&right.health.near_rank()))
        .then_with(|| compare_headroom_desc(left.headroom, right.headroom))
        .then_with(|| compare_observed_room(left.headroom, right.headroom))
        .then_with(|| cost_rank(left.tool.cost_class).cmp(&cost_rank(right.tool.cost_class)))
        .then_with(|| left.tool.name.cmp(&right.tool.name))
        .then_with(|| left.pool.cmp(&right.pool))
}

/// Least-used-first among pools that have no configured ceiling — the same
/// [`RoomBasis::ObservedUsage`] fallback the ledger's pool ranking uses.
///
/// Returns `Equal` unless BOTH sides are usage-derived, so it can never reorder
/// a pair that has real quota data, and is inert wherever no pools exist.
fn compare_observed_room(left: HeadroomScore, right: HeadroomScore) -> Ordering {
    match (left, right) {
        (
            HeadroomScore::Observed {
                tokens: left_tokens,
                requests: left_requests,
            },
            HeadroomScore::Observed {
                tokens: right_tokens,
                requests: right_requests,
            },
        ) => left_tokens
            .cmp(&right_tokens)
            .then_with(|| left_requests.cmp(&right_requests)),
        _ => Ordering::Equal,
    }
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
        "chose {} ({}, {}) for {preference} routing; mode={} headroom={}{}{} - {cost_note}",
        candidate.tool.name,
        cost_class_label(candidate.tool.cost_class),
        capability_label(req.capability),
        mode_label(candidate.mode),
        candidate.headroom.label(),
        health_suffix(candidate.health),
        basis_suffix(candidate.headroom),
    )
}

fn alternative_reason(candidate: &Candidate<'_>) -> String {
    format!(
        "{} candidate; fit={} mode={} headroom={}{}{}",
        cost_class_label(candidate.tool.cost_class),
        candidate.capability_fit,
        mode_label(candidate.mode),
        candidate.headroom.label(),
        health_suffix(candidate.health),
        basis_suffix(candidate.headroom),
    )
}

/// States what a pooled ordering was actually derived from, and only when that
/// is [`RoomBasis::ObservedUsage`] — a least-used-first fallback that must never
/// read as a quota measurement, because without a configured cap the remaining
/// quota is unknowable.
///
/// Empty for every capped candidate and for every candidate with no pool
/// identity, so reasons stay byte-identical wherever no pool is in play.
fn basis_suffix(headroom: HeadroomScore) -> String {
    match headroom.room_basis() {
        Some(RoomBasis::ObservedUsage) => {
            format!(" [pool order: {}]", RoomBasis::ObservedUsage.label())
        }
        Some(RoomBasis::Quota) | None => String::new(),
    }
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
            failover: false,
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

    /// Backward-compatibility lock. The four JSON blobs below were captured from
    /// the router as it stood *before* pool awareness and `failover` existed, on
    /// a mixed candidate set (local-free / subscription / metered / unknown-cost,
    /// with token caps, request caps, a near-limit target and two uncapped
    /// targets). With no pool in play and `failover` unset, every field of the
    /// decision — including every reason string — must still serialize to
    /// exactly these bytes.
    #[test]
    fn default_path_decisions_are_byte_identical() {
        const BASELINE: [&str; 4] = [
            r#"{"target":"ollama","mode":"cli","model":"qwen2.5-coder","cost_class":"LocalFree","reason":"chose ollama (local-free, code) for cheapest routing; mode=cli headroom=unknown - no API cost","alternatives":[{"target":"claude","mode":"cli","model":null,"cost_class":"SubscriptionFlat","capabilities":["Code"],"headroom":"500/1000 tokens remaining (50.0%)","reason":"subscription-flat candidate; fit=2 mode=cli headroom=500/1000 tokens remaining (50.0%)"},{"target":"openai","mode":"api","model":null,"cost_class":"ApiMetered","capabilities":["Code"],"headroom":"990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)","reason":"api-metered candidate; fit=2 mode=api headroom=990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)"},{"target":"anthropic","mode":"api","model":null,"cost_class":"ApiMetered","capabilities":["Code","Vision"],"headroom":"50/1000 tokens remaining (5.0%), 45/50 requests remaining (90.0%)","reason":"api-metered candidate; fit=3 mode=api headroom=50/1000 tokens remaining (5.0%), 45/50 requests remaining (90.0%) [near-limit: penalized]"},{"target":"gemini","mode":"cli","model":null,"cost_class":"Unknown","capabilities":["Code"],"headroom":"unknown","reason":"unknown-cost candidate; fit=2 mode=cli headroom=unknown"}]}"#,
            r#"{"target":"anthropic","mode":"api","model":null,"cost_class":"ApiMetered","reason":"chose anthropic (api-metered, code) for quality routing; mode=api headroom=50/1000 tokens remaining (5.0%), 45/50 requests remaining (90.0%) [near-limit: penalized] - metered cost","alternatives":[{"target":"openai","mode":"api","model":null,"cost_class":"ApiMetered","capabilities":["Code"],"headroom":"990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)","reason":"api-metered candidate; fit=2 mode=api headroom=990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)"},{"target":"claude","mode":"cli","model":null,"cost_class":"SubscriptionFlat","capabilities":["Code"],"headroom":"500/1000 tokens remaining (50.0%)","reason":"subscription-flat candidate; fit=2 mode=cli headroom=500/1000 tokens remaining (50.0%)"},{"target":"ollama","mode":"cli","model":"qwen2.5-coder","cost_class":"LocalFree","capabilities":["Code"],"headroom":"unknown","reason":"local-free candidate; fit=2 mode=cli headroom=unknown"},{"target":"gemini","mode":"cli","model":null,"cost_class":"Unknown","capabilities":["Code"],"headroom":"unknown","reason":"unknown-cost candidate; fit=2 mode=cli headroom=unknown"}]}"#,
            r#"{"target":"ollama","mode":"cli","model":"qwen2.5-coder","cost_class":"LocalFree","reason":"chose ollama (local-free, code) for local routing; mode=cli headroom=unknown - no API cost","alternatives":[{"target":"claude","mode":"cli","model":null,"cost_class":"SubscriptionFlat","capabilities":["Code"],"headroom":"500/1000 tokens remaining (50.0%)","reason":"subscription-flat candidate; fit=2 mode=cli headroom=500/1000 tokens remaining (50.0%)"},{"target":"openai","mode":"api","model":null,"cost_class":"ApiMetered","capabilities":["Code"],"headroom":"990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)","reason":"api-metered candidate; fit=2 mode=api headroom=990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)"},{"target":"anthropic","mode":"api","model":null,"cost_class":"ApiMetered","capabilities":["Code","Vision"],"headroom":"50/1000 tokens remaining (5.0%), 45/50 requests remaining (90.0%)","reason":"api-metered candidate; fit=3 mode=api headroom=50/1000 tokens remaining (5.0%), 45/50 requests remaining (90.0%) [near-limit: penalized]"},{"target":"gemini","mode":"cli","model":null,"cost_class":"Unknown","capabilities":["Code"],"headroom":"unknown","reason":"unknown-cost candidate; fit=2 mode=cli headroom=unknown"}]}"#,
            r#"{"target":"openai","mode":"api","model":null,"cost_class":"ApiMetered","reason":"explicit target 'openai' selected; mode=api cost=api-metered headroom=990/1000 tokens remaining (99.0%), 45/50 requests remaining (90.0%)","alternatives":[]}"#,
        ];

        let tools = mixed_tools();
        let usage = mixed_usage();
        let mut actual = Vec::new();
        for prefer in [Prefer::Cheapest, Prefer::Quality, Prefer::Local] {
            let decision =
                select_route(&request(Capability::Code, prefer), &tools, &usage).unwrap();
            actual.push(serde_json::to_string(&decision).unwrap());
        }
        let explicit = RouteRequest {
            capability: Some(Capability::Code),
            prefer: Prefer::Cheapest,
            target: Some("openai".to_string()),
            model: None,
            mode: None,
            failover: false,
        };
        actual.push(
            serde_json::to_string(&select_route(&explicit, &tools, &usage).unwrap()).unwrap(),
        );

        assert_eq!(actual, BASELINE.to_vec());
    }

    /// A request serialized before `failover` existed must still deserialize,
    /// and must still serialize back without the new key.
    #[test]
    fn route_request_wire_format_is_unchanged_by_default() {
        let legacy =
            r#"{"capability":"Code","prefer":"cheapest","target":null,"model":null,"mode":null}"#;
        let req: RouteRequest = serde_json::from_str(legacy).expect("legacy request parses");
        assert!(!req.failover);
        assert_eq!(serde_json::to_string(&req).unwrap(), legacy);
        assert!(!RouteRequest::default().failover);
    }

    #[test]
    fn exhausted_pool_ranks_below_its_sibling_pool() {
        // One target, two upstream pools. `opencode-go` has spent its own cap;
        // `ollama` has almost all of its own left. Keyed by target alone these
        // are indistinguishable — keyed by pool, the spent one is demoted.
        let tools = vec![pooled_tool()];
        let usage = pooled_usage(
            [("opencode#opencode-go", 400), ("opencode#ollama", 10)],
            [("opencode#opencode-go", 400), ("opencode#ollama", 1_000)],
        );
        let req = request(Capability::Code, Prefer::Cheapest);

        let decision = select_route(&req, &tools, &usage).unwrap();

        assert_eq!(decision.target, "opencode");
        assert_eq!(decision.model.as_deref(), Some(CLOUD_MODEL));
        let ranked = decision.ranked_targets();
        assert_eq!(ranked.len(), 2, "both pools stay reachable");
        let last = ranked.last().unwrap();
        assert_eq!(last.target, "opencode");
        assert_eq!(last.model.as_deref(), Some(GO_MODEL));
        assert!(
            decision.alternatives[0]
                .reason
                .contains("exhausted: demoted to last resort")
        );
    }

    #[test]
    fn shared_target_cap_is_not_split_between_pools() {
        // Only `[limits.opencode]` exists, so the two pools draw one pot down
        // together: both must see the WHOLE 1000 cap and the combined 750 draw,
        // never 500 each.
        let mut usage = pooled_usage(
            [("opencode#opencode-go", 150), ("opencode#ollama", 600)],
            [],
        );
        usage.usage_by_target.insert("opencode".into(), 750);
        usage.limits_by_target.insert("opencode".into(), 1_000);
        let tools = vec![pooled_tool()];

        let decision = select_route(&request(Capability::Code, Prefer::Cheapest), &tools, &usage)
            .expect("route");

        let headrooms = std::iter::once(headroom_of(&decision))
            .chain(decision.alternatives.iter().map(|alt| alt.headroom.clone()))
            .collect::<Vec<_>>();
        assert_eq!(headrooms.len(), 2);
        assert_eq!(
            headrooms[0], headrooms[1],
            "siblings share one pot, so they report the same room"
        );
        assert!(headrooms[0].contains("250/1000 tokens remaining"));
        assert!(headrooms[0].contains("cap shared with sibling pools"));
        // A split cap would have shown 500 apiece; that number must not exist.
        assert!(!headrooms[0].contains("/500"));
    }

    #[test]
    fn observed_usage_basis_is_disclosed_in_the_reason() {
        // No cap anywhere: the only signal is observed usage, so the least-used
        // pool wins — and the reason says the ordering is usage-derived rather
        // than presenting it as remaining quota.
        let tools = vec![pooled_tool()];
        let usage = pooled_usage(
            [("opencode#opencode-go", 900), ("opencode#ollama", 100)],
            [],
        );

        let decision = select_route(&request(Capability::Code, Prefer::Cheapest), &tools, &usage)
            .expect("route");

        assert_eq!(decision.model.as_deref(), Some(CLOUD_MODEL));
        assert!(
            decision
                .reason
                .contains("observed 24h usage, not a quota measurement"),
            "reason must state the basis: {}",
            decision.reason
        );
        assert!(
            decision.reason.contains("no configured cap"),
            "reason must not imply a ceiling exists: {}",
            decision.reason
        );
        assert!(
            !decision.reason.contains("remaining"),
            "usage-derived ordering must never claim remaining quota: {}",
            decision.reason
        );
        assert!(
            decision.alternatives[0]
                .reason
                .contains("observed 24h usage, not a quota measurement")
        );
    }

    #[test]
    fn explicit_target_without_failover_has_no_alternatives() {
        let tools = failover_tools();
        let mut req = request(Capability::Code, Prefer::Cheapest);
        req.target = Some("opencode".to_string());

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        assert!(decision.alternatives.is_empty());
        assert_eq!(decision.ranked_targets().len(), 1);
        assert!(!decision.reason.contains("failover"));
    }

    #[test]
    fn explicit_target_with_failover_keeps_the_target_first_then_falls_over() {
        // `--failover` with an explicit target used to be a no-op: one ranked
        // entry, nothing to fall over to. The explicit target still leads, but
        // now the rest of the ranking follows it, sibling pool first.
        let tools = failover_tools();
        let mut req = request(Capability::Code, Prefer::Cheapest);
        req.target = Some("opencode".to_string());
        req.failover = true;

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        let ranked = decision.ranked_targets();
        assert_eq!(ranked[0].target, "opencode");
        assert!(ranked.len() >= 2, "failover must have somewhere to go");
        // The sibling pool of the pinned target comes first: same target, other
        // upstream quota.
        assert_eq!(ranked[1].target, "opencode");
        assert_ne!(ranked[1].model, ranked[0].model);
        // ...and the other targets follow it.
        let tail = ranked[2..]
            .iter()
            .map(|entry| entry.target.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tail, vec!["ollama", "openai"]);
        assert!(decision.reason.contains("failover requested"));
    }

    #[test]
    fn failover_from_a_target_with_no_sibling_pool_still_falls_over() {
        let tools = failover_tools();
        let mut req = request(Capability::Code, Prefer::Cheapest);
        req.target = Some("openai".to_string());
        req.failover = true;

        let decision = select_route(&req, &tools, &UsageSnapshot::default()).unwrap();

        let ranked = decision.ranked_targets();
        assert_eq!(ranked[0].target, "openai");
        assert!(ranked.len() >= 2);
        assert!(ranked[1..].iter().all(|entry| entry.target != "openai"));
    }

    #[test]
    fn one_upstream_pool_never_fans_out() {
        // A model list that names at most one pool is a single upstream quota
        // however many tags it exposes, so it stays one candidate on its first
        // model — exactly as before pools existed.
        let mut single = pooled_tool();
        single.models = vec!["glm-5.2".to_string(), "opencode-go/glm-5.2".to_string()];
        let tools = vec![single];

        let decision = select_route(
            &request(Capability::Code, Prefer::Cheapest),
            &tools,
            &UsageSnapshot::default(),
        )
        .unwrap();

        assert_eq!(decision.model.as_deref(), Some("glm-5.2"));
        assert!(decision.alternatives.is_empty());
    }

    const GO_MODEL: &str = "opencode-go/glm-5.2";
    const CLOUD_MODEL: &str = "ollama/glm-5.2:cloud";

    /// The headroom clause of a primary decision's reason, without the trailing
    /// cost note, so it can be compared with an alternative's `headroom` field.
    fn headroom_of(decision: &RouteDecision) -> String {
        let (_, rest) = decision
            .reason
            .split_once("headroom=")
            .expect("reason carries headroom");
        rest.rsplit_once(" - ")
            .map(|(headroom, _cost_note)| headroom)
            .unwrap_or(rest)
            .to_string()
    }

    fn pooled_tool() -> DetectedTool {
        let mut detected = tool("opencode", CostClass::SubscriptionFlat, &[Capability::Code]);
        detected.models = vec![GO_MODEL.to_string(), CLOUD_MODEL.to_string()];
        detected
    }

    fn failover_tools() -> Vec<DetectedTool> {
        vec![
            pooled_tool(),
            local_tool("ollama", &[Capability::Code], &["qwen2.5-coder"]),
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
        ]
    }

    fn pooled_usage(
        usage: impl IntoIterator<Item = (&'static str, u64)>,
        limits: impl IntoIterator<Item = (&'static str, u64)>,
    ) -> UsageSnapshot {
        UsageSnapshot {
            usage_by_pool: usage
                .into_iter()
                .map(|(key, used)| (key.to_string(), used))
                .collect(),
            limits_by_pool: limits
                .into_iter()
                .map(|(key, limit)| (key.to_string(), limit))
                .collect(),
            ..UsageSnapshot::default()
        }
    }

    fn mixed_tools() -> Vec<DetectedTool> {
        vec![
            tool("openai", CostClass::ApiMetered, &[Capability::Code]),
            tool(
                "anthropic",
                CostClass::ApiMetered,
                &[Capability::Code, Capability::Vision],
            ),
            tool("claude", CostClass::SubscriptionFlat, &[Capability::Code]),
            local_tool("ollama", &[Capability::Code], &["qwen2.5-coder"]),
            tool("gemini", CostClass::Unknown, &[Capability::Code]),
        ]
    }

    fn mixed_usage() -> UsageSnapshot {
        UsageSnapshot::from_usage_limits_and_requests_for_tests(
            [("openai", 10), ("anthropic", 950), ("claude", 500)],
            [("openai", 1000), ("anthropic", 1000), ("claude", 1000)],
            [("openai", 5), ("anthropic", 5)],
            [("openai", 50), ("anthropic", 50)],
        )
    }

    fn request(capability: Capability, prefer: Prefer) -> RouteRequest {
        RouteRequest {
            capability: Some(capability),
            prefer,
            target: None,
            model: None,
            mode: None,
            failover: false,
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
