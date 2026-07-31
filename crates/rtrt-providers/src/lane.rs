//! The team dispatch loop: which lane runs a task, what happens when it fails,
//! and who actually ran it in the end.
//!
//! Until now this policy lived outside rtrt — in a prompt the operator pasted
//! into every session ("pick a lane by difficulty, check both pools before
//! assigning, cross to the sibling pool on a quota wall, retry a transient once,
//! halt on an auth error, redo from scratch after falling back, and report which
//! lane each task actually ran on"). Everything in that paragraph is mechanical,
//! so none of it belongs in a prompt. This module owns it:
//!
//! * [`resolve_lane`] turns a [`LaneTask`] into the ordered walk of
//!   [`LaneStep`]s the configured ladder implies — the lane, its sibling pool
//!   (ordered by which one actually has room), then its fallback chain.
//! * [`LaneRunner`] walks those steps under the shared failure classifier:
//!   transient earns one same-lane retry, quota crosses over immediately with no
//!   retry at all, and fatal halts the task rather than replaying a broken
//!   credential against every remaining lane.
//! * [`RedoDirective`] makes "redo from scratch" structural: the replacement
//!   lane's prompt is built from the ORIGINAL task text plus a directive naming
//!   what to rebuild, and there is no parameter through which a failed lane's
//!   partial output could reach it.
//! * [`LaneRun`] reports the lane each attempt was ASSIGNED to next to the lane
//!   that ACTUALLY ran it.
//!
//! Every bound comes from [`rtrt_core::TeamPolicy`] or is derived from the
//! roster; nothing here invents a limit.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use async_trait::async_trait;
use rtrt_core::{
    Config, CostClass, Error, PoolKey, Result, TeamConfig, TeamMember, TeamMode, TeamPolicy,
};
use serde::{Deserialize, Serialize};

use crate::{
    FailureClass, FailurePolicy, InvokeOptions, InvokeOutcome, Mode, PolicyAttempt, PolicyOutcome,
    RankedTarget, invoke_agent,
    usage_ledger::{PoolHeadroom, headroom_for_pool, rank_pools_by_room},
};

/// Appended to a room note when the ceiling behind it belongs to the target
/// rather than the pool. The cap is reported whole to both siblings and never
/// split into invented halves, so the note has to say so.
const SHARED_CAP_NOTE: &str = " (cap shared with sibling pools, reported whole, never divided)";

/// Stated when neither pool of a sibling pair has anything measured: the pair
/// keeps its configured order, and no room claim is made about it.
const NO_ROOM_NOTE: &str = "configured order (no room measured for either pool)";

/// The `[team]` mode of a lane, as the invoke layer's mode.
pub(crate) fn mode_from_team(mode: TeamMode) -> Mode {
    match mode {
        TeamMode::Cli => Mode::Cli,
        TeamMode::Api => Mode::Api,
        TeamMode::Auto => Mode::Auto,
    }
}

/// One unit of delegated work.
///
/// Deliberately small: an identity, the prompt, and the routing inputs the
/// ladder needs. Everything else about how it runs is `[team]` policy, not a
/// per-task knob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneTask {
    /// Stable label used in the provenance record and the one-line summary.
    pub id: String,
    /// The task text. This is the ONLY text a redo attempt is rebuilt from.
    pub prompt: String,
    /// Requested difficulty tier; `None` uses
    /// [`TeamConfig::effective_default_tier`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Whether the task writes code. Implementation tasks skip lanes that
    /// declare `allow_impl = false`, and may not be sent to a design-only tier.
    #[serde(default = "default_true")]
    pub implements: bool,
    /// Artifacts a redo must rebuild from scratch (files, functions, sections).
    /// Empty means "everything this task asks for".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// Per-task override of [`TeamPolicy::redo_on_fallback`]. `None` follows the
    /// policy. Set to `Some(false)` for a prompt that is already self-contained,
    /// so a lane change re-sends it byte for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_on_fallback: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl LaneTask {
    /// An implementation task at the policy's default tier.
    pub fn new(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            tier: None,
            implements: true,
            artifacts: Vec::new(),
            redo_on_fallback: None,
        }
    }

    /// A task that plans or reviews rather than edits, so design-only lanes stay
    /// eligible for it.
    pub fn design(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            implements: false,
            ..Self::new(id, prompt)
        }
    }

    pub fn with_tier(mut self, tier: impl Into<String>) -> Self {
        self.tier = Some(tier.into());
        self
    }

    /// Name what a redo has to rebuild. Without this the directive still says
    /// "start over", it just cannot be specific.
    pub fn with_artifacts<S: Into<String>>(
        mut self,
        artifacts: impl IntoIterator<Item = S>,
    ) -> Self {
        self.artifacts = artifacts.into_iter().map(Into::into).collect();
        self
    }

    /// Opt this task out of the redo directive (see
    /// [`LaneTask::redo_on_fallback`]).
    pub fn without_redo(mut self) -> Self {
        self.redo_on_fallback = Some(false);
        self
    }

    /// The tier this task routes through: its own, else the policy default.
    pub fn tier_for(&self, config: &TeamConfig) -> Option<String> {
        self.tier
            .clone()
            .or_else(|| config.effective_default_tier())
    }

    /// Whether a lane change must rebuild the work from scratch.
    pub fn redoes(&self, policy: &TeamPolicy) -> bool {
        self.redo_on_fallback.unwrap_or(policy.redo_on_fallback)
    }
}

/// Why a lane occupies its position in a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneRole {
    /// The first usable lane of the requested tier — the assignment itself.
    Primary,
    /// A later lane of the same tier, reached once the previous one's whole
    /// group was spent.
    Alternate,
    /// The sibling pool of this step's assigned lane: the same logical model on
    /// a different quota. Sideways, so a quota wall costs a pool switch instead
    /// of a model downgrade.
    Sibling,
    /// A replacement lane from the assigned lane's `fallback` chain — usually a
    /// different model, hence tried only after the sibling pool.
    Fallback,
}

impl LaneRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Alternate => "alternate",
            Self::Sibling => "sibling",
            Self::Fallback => "fallback",
        }
    }
}

/// One rung of a task's walk: a concrete lane, plus which assignment it stands
/// in for and why it sits here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneStep {
    /// Team member name of the lane that runs this step.
    pub lane: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub mode: Mode,
    /// The lane the ladder ASSIGNED this task to. Equal to `lane` for a
    /// [`LaneRole::Primary`] / [`LaneRole::Alternate`] step, and the lane being
    /// stood in for otherwise.
    pub assigned: String,
    pub role: LaneRole,
    /// Operator-readable justification. When the order came from a room
    /// comparison it names the basis verbatim, so a usage-derived ordering can
    /// never be misread as a quota measurement.
    pub reason: String,
}

impl LaneStep {
    /// The invoke-layer candidate this step names.
    pub fn ranked_target(&self) -> RankedTarget {
        RankedTarget {
            target: self.target.clone(),
            mode: self.mode,
            model: self.model.clone(),
            cost_class: CostClass::Unknown,
        }
    }

    /// The quota bucket this step draws from.
    pub fn pool_key(&self) -> PoolKey {
        PoolKey::from_target_model(&self.target, self.model.as_deref())
    }

    fn from_member(member: &TeamMember, assigned: &str, role: LaneRole, reason: String) -> Self {
        Self {
            lane: member.name.clone(),
            target: member.target.clone(),
            model: member.model.clone(),
            mode: mode_from_team(member.mode),
            assigned: assigned.to_string(),
            role,
            reason,
        }
    }
}

/// Where the walk gets its "which of these two pools has room" answer.
///
/// Injected so a lane order can be decided (and tested) without a usage ledger
/// on disk, and so a caller that already holds a snapshot does not re-read one.
pub trait LaneRoom: Send + Sync {
    /// 24h headroom for one pool, or `None` when nothing is known about it.
    /// `None` is not "empty" — it means no claim can be made, and the walk then
    /// keeps the configured order instead of inventing one.
    fn headroom(&self, key: &PoolKey) -> Option<PoolHeadroom>;
}

/// No measurement at all: lanes keep the order the config declares.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnknownRoom;

impl LaneRoom for UnknownRoom {
    fn headroom(&self, _key: &PoolKey) -> Option<PoolHeadroom> {
        None
    }
}

/// The default room source, used whenever a runner is not given another.
pub const UNKNOWN_ROOM: UnknownRoom = UnknownRoom;

/// The real answer: the on-disk usage ledger read against the configured
/// `[limits]`, which is what decides whether the basis is a quota fraction or
/// merely observed usage.
#[derive(Debug, Clone)]
pub struct LedgerRoom {
    config: Config,
}

impl LedgerRoom {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Read the effective (global ⊕ project) config for the current directory.
    pub fn from_effective_config() -> Self {
        Self::new(Config::load_effective_for_cwd())
    }
}

impl LaneRoom for LedgerRoom {
    fn headroom(&self, key: &PoolKey) -> Option<PoolHeadroom> {
        Some(headroom_for_pool(key, &self.config))
    }
}

/// A fixed snapshot: for callers holding a `pool_headroom` map already, and for
/// tests that must not touch the ledger.
#[derive(Debug, Clone, Default)]
pub struct StaticRoom {
    pools: BTreeMap<String, PoolHeadroom>,
}

impl StaticRoom {
    pub fn new(pools: impl IntoIterator<Item = PoolHeadroom>) -> Self {
        Self {
            pools: pools
                .into_iter()
                .map(|pool| (pool.key.clone(), pool))
                .collect(),
        }
    }
}

impl LaneRoom for StaticRoom {
    fn headroom(&self, key: &PoolKey) -> Option<PoolHeadroom> {
        self.pools.get(&key.canonical()).cloned()
    }
}

/// The directive that turns a lane change into a rebuild rather than a resume.
///
/// A lane that died mid-task usually left half-written artifacts, so the
/// replacement must not continue from them. [`RedoDirective::apply`] takes the
/// ORIGINAL task text and nothing else — there is deliberately no parameter
/// through which the failed lane's partial output could be threaded in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedoDirective {
    /// The lane whose attempt was abandoned.
    pub from_lane: String,
    /// The lane that must start over.
    pub to_lane: String,
    /// What to rebuild. Empty means "everything this task asks for".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

impl RedoDirective {
    /// The directive text on its own.
    pub fn render(&self) -> String {
        let what = if self.artifacts.is_empty() {
            "every artifact this task asks for".to_string()
        } else {
            self.artifacts.join(", ")
        };
        format!(
            "<redo_from_scratch>\n\
             Lane {} did not finish this task; its work is unusable and is not included above.\n\
             Rebuild from scratch: {what}.\n\
             Do not resume, patch or append to any partial result — assume nothing usable exists \
             and produce the complete artifact yourself.\n\
             </redo_from_scratch>",
            self.from_lane
        )
    }

    /// The prompt for the redo attempt: the original task text plus the
    /// directive. The signature is the enforcement — see the type docs.
    pub fn apply(&self, original_prompt: &str) -> String {
        format!("{original_prompt}\n\n{}", self.render())
    }
}

/// One invocation of one lane, with the assignment it stood in for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneAttempt {
    /// The lane the ladder assigned the task to.
    pub assigned: String,
    /// The lane that actually ran this attempt.
    pub actual: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub role: LaneRole,
    /// Why this lane was next (see [`LaneStep::reason`]).
    pub reason: String,
    /// How it failed; `None` when this attempt served the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<FailureClass>,
    /// True when this attempt IS the same-lane retry of a transient failure.
    #[serde(default)]
    pub retried: bool,
    /// The redo directive this attempt was issued, when the walk changed lanes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo: Option<RedoDirective>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl LaneAttempt {
    fn provenance_entry(&self) -> String {
        let mut entry = if self.assigned == self.actual {
            format!("{} ran {}", self.assigned, self.role.label())
        } else {
            format!(
                "{} assigned, {} ran ({})",
                self.assigned,
                self.actual,
                self.role.label()
            )
        };
        match self.class {
            Some(class) => entry.push_str(&format!(": {}", class.label())),
            None => entry.push_str(": served"),
        }
        if self.retried {
            entry.push_str(", retry");
        }
        if self.redo.is_some() {
            entry.push_str(", redo issued");
        }
        entry
    }
}

/// The lane whose fatal failure ended a task.
///
/// A halt is always [`FailureClass::Fatal`]: an expired credential or a bad
/// model id is operator-actionable, so no later lane is tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneHalt {
    /// The lane that halted the task.
    pub lane: String,
    /// The assignment it was standing in for.
    pub assigned: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Whether a same-lane retry was consumed before the fatal failure.
    #[serde(default)]
    pub retried: bool,
    pub error: String,
}

/// What happened to one task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneRun {
    pub task_id: String,
    /// The lane the walk started at — what the ladder assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
    /// The lane that actually served the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served: Option<InvokeOutcome>,
    /// Per-attempt provenance, in walk order. Gated on
    /// [`TeamPolicy::record_provenance`]: empty when the operator turned
    /// reporting off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<LaneAttempt>,
    /// The compact failure trail, one entry per lane that failed, retries
    /// collapsed. Always recorded regardless of `record_provenance`, because the
    /// aggregated error a caller surfaces is built from it — suppressing a
    /// report must never blank out an error message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trail: Vec<PolicyAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halted: Option<LaneHalt>,
    /// Same-lane retries consumed, against [`TeamPolicy::max_retries`].
    #[serde(default)]
    pub retries_used: u32,
    /// Why the task never started: no tier, no usable lane, an empty walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<String>,
}

impl LaneRun {
    fn new(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            assigned: None,
            served_by: None,
            served: None,
            attempts: Vec::new(),
            trail: Vec::new(),
            halted: None,
            retries_used: 0,
            unresolved: None,
        }
    }

    pub fn succeeded(&self) -> bool {
        self.served.is_some()
    }

    /// One line per task, suitable for a final report: what was assigned, what
    /// actually ran, and how it ended.
    pub fn summary(&self) -> String {
        let head = match (&self.assigned, &self.served_by) {
            (Some(assigned), Some(actual)) if assigned == actual => {
                format!("{}: {assigned}", self.task_id)
            }
            (Some(assigned), Some(actual)) => {
                format!("{}: assigned {assigned}, ran {actual}", self.task_id)
            }
            (Some(assigned), None) => format!("{}: assigned {assigned}", self.task_id),
            (None, _) => self.task_id.clone(),
        };
        if let Some(reason) = &self.unresolved {
            return format!("{head} — not dispatched: {reason}");
        }
        if let Some(halt) = &self.halted {
            return format!(
                "{head} — halted at {}: fatal, no failover ({})",
                halt.lane, halt.error
            );
        }
        let trail = self
            .attempts
            .iter()
            .map(LaneAttempt::provenance_entry)
            .collect::<Vec<_>>()
            .join("; ");
        let detail = if trail.is_empty() {
            String::new()
        } else {
            format!(" [{trail}]")
        };
        match &self.served_by {
            Some(_) if self.trail.is_empty() => format!("{head} — served{detail}"),
            Some(_) => format!(
                "{head} — served after {} lane(s) fell over{detail}",
                self.trail.len()
            ),
            None => format!(
                "{head} — all {} lane(s) failed{detail}",
                self.trail.len().max(1)
            ),
        }
    }

    /// Flatten onto the invoke layer's walk record, so a caller that already
    /// speaks [`PolicyOutcome`] (and through it `FailoverOutcome`) keeps its
    /// exact error and trail contract.
    pub fn into_policy_outcome(self) -> PolicyOutcome {
        let halted = self.halted.map(|halt| PolicyAttempt {
            target: halt.target,
            model: halt.model,
            class: FailureClass::Fatal,
            retried: halt.retried,
            error: halt.error,
        });
        PolicyOutcome {
            served: self.served,
            attempts: self.trail,
            halted,
        }
    }
}

/// Every task's outcome, plus a compact whole-run summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneReport {
    pub runs: Vec<LaneRun>,
}

impl LaneReport {
    pub fn all_served(&self) -> bool {
        self.runs.iter().all(LaneRun::succeeded)
    }

    /// Tasks a fatal failure stopped. These need an operator, not a retry.
    pub fn halted(&self) -> impl Iterator<Item = &LaneRun> {
        self.runs.iter().filter(|run| run.halted.is_some())
    }

    /// One line per task.
    pub fn summary(&self) -> String {
        self.runs
            .iter()
            .map(LaneRun::summary)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// How one lane invocation is actually performed.
///
/// The retry / crossover / halt policy lives in the walk, so an implementation
/// must perform exactly ONE attempt and must not retry on its own.
#[async_trait]
pub trait LaneInvoker: Send + Sync {
    async fn invoke(
        &self,
        step: &LaneStep,
        prompt: &str,
        timeout: Duration,
    ) -> Result<InvokeOutcome>;
}

/// The production invoker: one `invoke_agent` call per attempt.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentInvoker;

#[async_trait]
impl LaneInvoker for AgentInvoker {
    async fn invoke(
        &self,
        step: &LaneStep,
        prompt: &str,
        timeout: Duration,
    ) -> Result<InvokeOutcome> {
        let opts = InvokeOptions {
            mode: Some(step.mode),
            model: step.model.clone(),
            timeout,
        };
        invoke_agent(&step.target, prompt, opts).await
    }
}

/// The default invoker, used whenever a runner is not given another.
pub const AGENT_INVOKER: AgentInvoker = AgentInvoker;

/// The order of a lane and its sibling pool, plus what decided it.
struct RoomOrder {
    sibling_first: bool,
    note: String,
}

/// Decide between a lane and its sibling pool by remaining room.
///
/// The note always names the basis verbatim, because the two bases mean very
/// different things: [`crate::RoomBasis::Quota`] is a real remaining fraction,
/// while `ObservedUsage` is only "this pool has been used less lately" and must
/// never be presented as a quota measurement. A cap that lives at the target
/// level is flagged as shared rather than divided between the siblings.
fn order_by_room(room: &dyn LaneRoom, primary: &TeamMember, sibling: &TeamMember) -> RoomOrder {
    let primary_key = PoolKey::from_target_model(&primary.target, primary.model.as_deref());
    let sibling_key = PoolKey::from_target_model(&sibling.target, sibling.model.as_deref());
    if primary_key == sibling_key {
        // Same bucket: there is nothing to compare, and claiming otherwise would
        // dress a coin flip up as a measurement.
        return RoomOrder {
            sibling_first: false,
            note: "configured order (both lanes draw on the same pool)".to_string(),
        };
    }
    let (Some(primary_room), Some(sibling_room)) =
        (room.headroom(&primary_key), room.headroom(&sibling_key))
    else {
        return RoomOrder {
            sibling_first: false,
            note: NO_ROOM_NOTE.to_string(),
        };
    };

    let shared = primary_room.shares_a_cap() || sibling_room.shares_a_cap();
    let sibling_key = sibling_room.key.clone();
    let ranking = rank_pools_by_room(&[primary_room, sibling_room]);
    let sibling_first = ranking.best().is_some_and(|best| best.key == sibling_key);
    let mut note = format!("ranked by remaining room: {}", ranking.basis.label());
    if shared {
        note.push_str(SHARED_CAP_NOTE);
    }
    RoomOrder {
        sibling_first,
        note,
    }
}

/// The inputs a walk expansion needs, bundled so the recursion stays readable.
struct WalkContext<'a> {
    config: &'a TeamConfig,
    room: &'a dyn LaneRoom,
    /// True when the task writes code, which is what makes `allow_impl` bite.
    implements: bool,
}

impl WalkContext<'_> {
    fn usable(&self, member: &TeamMember) -> bool {
        !self.implements || member.allow_impl
    }

    /// Expand one assignment into its walk: the lane and its sibling pool
    /// (roomiest first), then the lane's fallback chain.
    fn push_group(
        &self,
        head: &str,
        head_role: LaneRole,
        head_reason: &str,
        seen: &mut BTreeSet<String>,
        steps: &mut Vec<LaneStep>,
    ) {
        let Some(primary) = self.config.member(head) else {
            return;
        };

        // Sideways before up: the sibling pool is the same logical model on a
        // different quota, so crossing to it costs a pool switch, while every
        // fallback entry usually costs a model downgrade.
        let sibling = self
            .config
            .policy
            .prefer_sibling_on_quota
            .then(|| self.config.sibling_of(head))
            .flatten()
            .filter(|sibling| self.usable(sibling));

        match sibling {
            Some(sibling) => {
                let order = order_by_room(self.room, primary, sibling);
                let primary_step = LaneStep::from_member(
                    primary,
                    head,
                    head_role,
                    format!("{head_reason}; {}", order.note),
                );
                let sibling_step = LaneStep::from_member(
                    sibling,
                    head,
                    LaneRole::Sibling,
                    format!(
                        "sibling pool of {head} (same logical model, different quota); {}",
                        order.note
                    ),
                );
                let pair = if order.sibling_first {
                    [sibling_step, primary_step]
                } else {
                    [primary_step, sibling_step]
                };
                for step in pair {
                    push_step(step, seen, steps);
                }
            }
            None => push_step(
                LaneStep::from_member(primary, head, head_role, head_reason.to_string()),
                seen,
                steps,
            ),
        }

        // `fallback_chain` is already cut at `effective_max_fallback_depth`,
        // which derives from the roster unless the policy pins it.
        for (position, name) in self.config.fallback_chain(head).into_iter().enumerate() {
            let Some(member) = self.config.member(&name) else {
                continue;
            };
            if !self.usable(member) {
                continue;
            }
            push_step(
                LaneStep::from_member(
                    member,
                    head,
                    LaneRole::Fallback,
                    format!("fallback {} of {head}", position + 1),
                ),
                seen,
                steps,
            );
        }
    }
}

/// Append a step unless its lane already appears in the walk. A lane keeps the
/// earliest position it earned, so a fallback entry never demotes a tier lane.
fn push_step(step: LaneStep, seen: &mut BTreeSet<String>, steps: &mut Vec<LaneStep>) {
    if seen.insert(step.lane.clone()) {
        steps.push(step);
    }
}

/// The ordered walk a task's tier implies.
///
/// Per tier lane, in configured order: the lane itself, its sibling pool when
/// [`TeamPolicy::prefer_sibling_on_quota`] is set (with the two ordered by
/// remaining room), then the lane's `fallback` chain. Lanes that cannot
/// implement are skipped for an implementation task, and a lane already in the
/// walk is never repeated.
pub fn resolve_lane(
    config: &TeamConfig,
    task: &LaneTask,
    room: &dyn LaneRoom,
) -> Result<Vec<LaneStep>> {
    let Some(tier) = task.tier_for(config) else {
        return Err(Error::Config(format!(
            "team task {}: no tier requested and the roster declares no ladder",
            task.id
        )));
    };
    let tiers = config.effective_tiers();
    let Some(lanes) = tiers.get(&tier) else {
        return Err(Error::Config(format!(
            "team task {}: no lane serves tier {tier}",
            task.id
        )));
    };
    if task.implements && config.is_design_only_tier(&tier) {
        return Err(Error::Config(format!(
            "team task {}: tier {tier} is design-only, so it cannot take an implementation task",
            task.id
        )));
    }

    let context = WalkContext {
        config,
        room,
        implements: task.implements,
    };
    let usable: Vec<&String> = lanes
        .iter()
        .filter(|name| {
            config
                .member(name)
                .is_some_and(|member| context.usable(member))
        })
        .collect();
    if usable.is_empty() {
        return Err(Error::Config(format!(
            "team task {}: every lane of tier {tier} is design-only",
            task.id
        )));
    }

    let mut steps = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, name) in usable.iter().enumerate() {
        let (role, reason) = if index == 0 {
            (LaneRole::Primary, format!("first lane of tier {tier}"))
        } else {
            (
                LaneRole::Alternate,
                format!("alternate {} of tier {tier}", index + 1),
            )
        };
        context.push_group(name, role, &reason, &mut seen, &mut steps);
    }
    Ok(steps)
}

/// The walk `dispatch_team` uses for the LEADER.
///
/// Leaders come from `leader_order`, not from the ladder, but each one is
/// expanded exactly like a tier assignment: the leader, its sibling pool, then
/// its fallback chain. A roster that declares no `sibling` and no `fallback`
/// therefore collapses to `leader_order` itself, which is what dispatch did
/// before lanes existed.
pub fn resolve_leader_lane(config: &TeamConfig, room: &dyn LaneRoom) -> Result<Vec<LaneStep>> {
    // A leader plans and integrates rather than edits, so `allow_impl` must not
    // filter it out — the shipped roster's first leader is design-only.
    let context = WalkContext {
        config,
        room,
        implements: false,
    };
    let mut steps = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, leader) in config.leader_order.iter().enumerate() {
        if config.member(leader).is_none() {
            return Err(Error::Config(format!(
                "team leader references unknown member: {leader}"
            )));
        }
        let (role, reason) = if index == 0 {
            (LaneRole::Primary, "first configured leader".to_string())
        } else {
            (
                LaneRole::Alternate,
                format!("configured leader {}", index + 1),
            )
        };
        context.push_group(leader, role, &reason, &mut seen, &mut steps);
    }
    Ok(steps)
}

/// Everything a walk needs besides the tasks: who invokes, what room
/// information the order is decided on, how failures classify, and the per-call
/// timeout.
pub struct LaneRunner<'a> {
    config: &'a TeamConfig,
    invoker: &'a dyn LaneInvoker,
    room: &'a dyn LaneRoom,
    failure: FailurePolicy,
    timeout: Duration,
}

impl<'a> LaneRunner<'a> {
    /// A runner with no room information, the built-in failure markers, and the
    /// default per-call timeout.
    pub fn new(config: &'a TeamConfig, invoker: &'a dyn LaneInvoker) -> Self {
        Self {
            config,
            invoker,
            room: &UNKNOWN_ROOM,
            failure: FailurePolicy::builtin(),
            timeout: Duration::from_secs(crate::DEFAULT_TIMEOUT_SECS),
        }
    }

    pub fn with_room(mut self, room: &'a dyn LaneRoom) -> Self {
        self.room = room;
        self
    }

    /// Layer the user's `[failover]` markers and backoff over the built-ins.
    pub fn with_failure_policy(mut self, failure: FailurePolicy) -> Self {
        self.failure = failure;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The walk this runner would take for a task.
    pub fn resolve(&self, task: &LaneTask) -> Result<Vec<LaneStep>> {
        resolve_lane(self.config, task, self.room)
    }

    /// Resolve and run one task.
    pub async fn run_task(&self, task: &LaneTask) -> LaneRun {
        match self.resolve(task) {
            Ok(steps) => self.run_steps(task, &steps).await,
            Err(err) => {
                let mut run = LaneRun::new(&task.id);
                run.unresolved = Some(err.to_string());
                run
            }
        }
    }

    /// Run every task in order. A task that halts stops ITS walk only: the next
    /// task still runs, because a broken credential on one lane says nothing
    /// about an independent unit of work.
    pub async fn run_tasks(&self, tasks: &[LaneTask]) -> LaneReport {
        let mut runs = Vec::with_capacity(tasks.len());
        for task in tasks {
            runs.push(self.run_task(task).await);
        }
        LaneReport { runs }
    }

    /// Walk a pre-resolved step list.
    ///
    /// Per step, the shared classifier decides what happens next:
    /// * [`FailureClass::Transient`] — one backed-off retry on the SAME lane
    ///   (while the task's retry budget allows), then advance.
    /// * [`FailureClass::Quota`] — advance immediately, with no retry at all:
    ///   the lane is out of allowance, so retrying it only adds latency.
    /// * [`FailureClass::Fatal`] — stop the task. No later lane is tried, and
    ///   the halt names the lane.
    pub async fn run_steps(&self, task: &LaneTask, steps: &[LaneStep]) -> LaneRun {
        let policy = &self.config.policy;
        let record = policy.record_provenance;
        let mut run = LaneRun::new(&task.id);
        run.assigned = steps.first().map(|step| step.assigned.clone());
        if steps.is_empty() {
            run.unresolved = Some(format!("team task {}: the walk is empty", task.id));
            return run;
        }

        // The cap is on the TASK, not on each lane: a walk that retried every
        // lane it touched would multiply the configured budget by the roster.
        let mut retry_budget = policy.max_retries;
        let mut previous_lane: Option<&str> = None;

        for step in steps {
            let redo = match previous_lane {
                Some(previous) if previous != step.lane && task.redoes(policy) => {
                    Some(RedoDirective {
                        from_lane: previous.to_string(),
                        to_lane: step.lane.clone(),
                        artifacts: task.artifacts.clone(),
                    })
                }
                _ => None,
            };
            // Built from the ORIGINAL task text either way: no attempt ever sees
            // an earlier lane's output.
            let prompt = match &redo {
                Some(directive) => directive.apply(&task.prompt),
                None => task.prompt.clone(),
            };

            let mut retried = false;
            loop {
                match self.invoker.invoke(step, &prompt, self.timeout).await {
                    Ok(outcome) => {
                        if record {
                            run.attempts
                                .push(attempt_of(step, redo.clone(), retried, None, None));
                        }
                        run.served_by = Some(step.lane.clone());
                        run.served = Some(outcome);
                        return run;
                    }
                    Err(err) => {
                        let class = self.failure.classify(&err);
                        let error = err.to_string();
                        if record {
                            run.attempts.push(attempt_of(
                                step,
                                redo.clone(),
                                retried,
                                Some(class),
                                Some(error.clone()),
                            ));
                        }
                        push_trail(&mut run.trail, step, class, retried, &error);

                        if class == FailureClass::Fatal {
                            run.halted = Some(LaneHalt {
                                lane: step.lane.clone(),
                                assigned: step.assigned.clone(),
                                target: step.target.clone(),
                                model: step.model.clone(),
                                retried,
                                error,
                            });
                            return run;
                        }
                        // One same-lane retry per transient failure, mirroring
                        // the invoke layer, and only while the task's budget
                        // still has room.
                        if class == FailureClass::Transient && !retried && retry_budget > 0 {
                            retry_budget -= 1;
                            run.retries_used += 1;
                            retried = true;
                            tokio::time::sleep(self.failure.backoff(self.timeout)).await;
                            continue;
                        }
                        previous_lane = Some(&step.lane);
                        break;
                    }
                }
            }
        }
        run
    }
}

fn attempt_of(
    step: &LaneStep,
    redo: Option<RedoDirective>,
    retried: bool,
    class: Option<FailureClass>,
    error: Option<String>,
) -> LaneAttempt {
    LaneAttempt {
        assigned: step.assigned.clone(),
        actual: step.lane.clone(),
        target: step.target.clone(),
        model: step.model.clone(),
        role: step.role,
        reason: step.reason.clone(),
        class,
        retried,
        redo,
        error,
    }
}

/// Append to the compact trail, collapsing a same-lane retry into the entry it
/// retried — the shape `PolicyOutcome` (and through it the aggregated failover
/// error) has always had.
fn push_trail(
    trail: &mut Vec<PolicyAttempt>,
    step: &LaneStep,
    class: FailureClass,
    retried: bool,
    error: &str,
) {
    if retried
        && let Some(last) = trail.last_mut()
        && last.target == step.target
        && last.model == step.model
    {
        last.class = class;
        last.retried = true;
        last.error = error.to_string();
        return;
    }
    trail.push(PolicyAttempt {
        target: step.target.clone(),
        model: step.model.clone(),
        class,
        retried,
        error: error.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use rtrt_core::{TierMap, config::FailoverConfig};

    use super::*;
    use crate::usage_ledger::{CapScope, PoolCap};

    /// A distinctive marker planted in a failing lane's message. If it ever
    /// reaches a later prompt, partial output leaked into the redo.
    const PARTIAL: &str = "PARTIAL-OUTPUT-MARKER-b7f3";

    /// One scripted lane invocation: success text, or an error message that the
    /// shared classifier will bucket.
    type Scripted = std::result::Result<String, String>;

    /// A fake invoker: per-lane queued outcomes plus a full call log. Performs
    /// exactly one attempt per call, like the real one.
    #[derive(Default)]
    struct ScriptedInvoker {
        script: Mutex<BTreeMap<String, VecDeque<Scripted>>>,
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl ScriptedInvoker {
        fn new(script: impl IntoIterator<Item = (&'static str, Vec<Scripted>)>) -> Self {
            Self {
                script: Mutex::new(
                    script
                        .into_iter()
                        .map(|(lane, outcomes)| (lane.to_string(), outcomes.into()))
                        .collect(),
                ),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn lanes_called(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(lane, _)| lane.clone())
                .collect()
        }

        fn prompts(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, prompt)| prompt.clone())
                .collect()
        }
    }

    #[async_trait]
    impl LaneInvoker for ScriptedInvoker {
        async fn invoke(
            &self,
            step: &LaneStep,
            prompt: &str,
            _timeout: Duration,
        ) -> Result<InvokeOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push((step.lane.clone(), prompt.to_string()));
            let scripted = self
                .script
                .lock()
                .unwrap()
                .get_mut(&step.lane)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| Ok(format!("output from {}", step.lane)));
            match scripted {
                Ok(output) => Ok(InvokeOutcome {
                    target: step.target.clone(),
                    mode_used: step.mode,
                    model: step.model.clone(),
                    output,
                    exit_code: Some(0),
                    ms: 0,
                }),
                Err(message) => Err(Error::Provider(message)),
            }
        }
    }

    fn quota(lane: &str) -> Scripted {
        Err(format!("{lane} refused: 429 rate limit, {PARTIAL}"))
    }

    fn transient(lane: &str) -> Scripted {
        Err(format!("{lane} timed out, {PARTIAL}"))
    }

    fn fatal(lane: &str) -> Scripted {
        Err(format!("{lane}: 401 unauthorized, {PARTIAL}"))
    }

    fn lane(name: &str, target: &str, model: &str, logical: &str) -> TeamMember {
        TeamMember {
            model: Some(model.to_string()),
            roles: vec!["impl".to_string()],
            logical: Some(logical.to_string()),
            ..TeamMember::new(name, target, TeamMode::Cli)
        }
    }

    /// A sibling pair on one target (`glm-go` / `glm-cloud`, two upstream pools
    /// of the same logical model) plus an unrelated fallback lane.
    fn config() -> TeamConfig {
        let mut go = lane("glm-go", "opencode", "opencode-go/glm-5.2", "glm-5.2");
        go.sibling = Some("glm-cloud".to_string());
        go.fallback = vec!["sonnet".to_string()];
        let mut cloud = lane("glm-cloud", "opencode", "ollama/glm-5.2:cloud", "glm-5.2");
        cloud.sibling = Some("glm-go".to_string());

        TeamConfig {
            enabled: true,
            leader_order: vec!["glm-go".to_string()],
            members: vec![go, cloud, lane("sonnet", "claude", "sonnet", "sonnet")],
            tiers: TierMap::from_pairs([("routine", vec!["glm-go"])]),
            ..TeamConfig::default()
        }
    }

    /// Zero backoff so the retry tests do not sit on a timer.
    fn instant_policy() -> FailurePolicy {
        FailurePolicy::from_config(&FailoverConfig {
            backoff_ms: Some(0),
            ..FailoverConfig::default()
        })
    }

    fn runner<'a>(config: &'a TeamConfig, invoker: &'a ScriptedInvoker) -> LaneRunner<'a> {
        LaneRunner::new(config, invoker)
            .with_failure_policy(instant_policy())
            .with_timeout(Duration::from_millis(50))
    }

    fn task() -> LaneTask {
        LaneTask::new("t1", "ORIGINAL TASK TEXT").with_artifacts(["src/lib.rs"])
    }

    fn pool(
        key: &str,
        target: &str,
        name: &str,
        used_tokens: u64,
        limit: Option<u64>,
    ) -> PoolHeadroom {
        let cap = match limit {
            Some(limit) => PoolCap {
                scope: CapScope::Pool,
                limit: Some(limit),
                used: used_tokens,
                remaining: Some(limit.saturating_sub(used_tokens)),
            },
            None => PoolCap {
                scope: CapScope::Unknown,
                limit: None,
                used: used_tokens,
                remaining: None,
            },
        };
        PoolHeadroom {
            key: key.to_string(),
            target: target.to_string(),
            pool: Some(name.to_string()),
            used_tokens,
            used_requests: 0,
            tokens_estimated: false,
            tokens: cap,
            requests: PoolCap {
                scope: CapScope::Unknown,
                limit: None,
                used: 0,
                remaining: None,
            },
            sibling_pools: 2,
        }
    }

    #[test]
    fn the_walk_is_lane_then_sibling_then_fallback() {
        let config = config();
        config.validate().unwrap();
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();

        let walk: Vec<(&str, LaneRole, &str)> = steps
            .iter()
            .map(|step| (step.lane.as_str(), step.role, step.assigned.as_str()))
            .collect();
        assert_eq!(
            walk,
            vec![
                ("glm-go", LaneRole::Primary, "glm-go"),
                ("glm-cloud", LaneRole::Sibling, "glm-go"),
                ("sonnet", LaneRole::Fallback, "glm-go"),
            ]
        );
    }

    #[tokio::test]
    async fn quota_crosses_to_the_sibling_pool_without_retrying() {
        let config = config();
        let invoker = ScriptedInvoker::new([("glm-go", vec![quota("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        // Zero retries on the exhausted lane: it is out of allowance, not flaky.
        assert_eq!(invoker.lanes_called(), vec!["glm-go", "glm-cloud"]);
        assert_eq!(run.retries_used, 0);
        assert_eq!(run.served_by.as_deref(), Some("glm-cloud"));
        assert_eq!(run.attempts[0].class, Some(FailureClass::Quota));
        assert!(!run.attempts[0].retried);
        assert_eq!(run.attempts[1].assigned, "glm-go");
        assert_eq!(run.attempts[1].actual, "glm-cloud");
        assert_eq!(run.attempts[1].role, LaneRole::Sibling);
    }

    #[tokio::test]
    async fn transient_earns_exactly_one_same_lane_retry() {
        let config = config();
        let invoker =
            ScriptedInvoker::new([("glm-go", vec![transient("glm-go"), transient("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        assert_eq!(
            invoker.lanes_called(),
            vec!["glm-go", "glm-go", "glm-cloud"]
        );
        assert_eq!(run.retries_used, 1);
        assert!(!run.attempts[0].retried);
        assert!(run.attempts[1].retried);
        assert_eq!(run.served_by.as_deref(), Some("glm-cloud"));
        // The compact trail collapses the retry into one lane entry.
        assert_eq!(run.trail.len(), 1);
        assert!(run.trail[0].retried);
    }

    #[tokio::test]
    async fn fatal_stops_the_walk_and_names_the_lane() {
        let config = config();
        let invoker = ScriptedInvoker::new([("glm-go", vec![fatal("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        assert_eq!(invoker.lanes_called(), vec!["glm-go"]);
        let halt = run.halted.as_ref().expect("fatal must halt the task");
        assert_eq!(halt.lane, "glm-go");
        assert_eq!(halt.assigned, "glm-go");
        assert!(halt.error.contains("401 unauthorized"));
        assert!(run.served.is_none());
        assert!(
            run.summary()
                .contains("halted at glm-go: fatal, no failover")
        );
    }

    #[tokio::test]
    async fn a_fatal_task_does_not_block_an_independent_one() {
        let config = config();
        let invoker = ScriptedInvoker::new([("glm-go", vec![fatal("glm-go")])]);
        let doomed = LaneTask::new("halts", "first task");
        let healthy = LaneTask::new("ok", "second task");
        let report = runner(&config, &invoker)
            .run_tasks(&[doomed, healthy])
            .await;

        assert!(report.runs[0].halted.is_some());
        assert!(report.runs[1].succeeded());
        assert_eq!(report.runs[1].served_by.as_deref(), Some("glm-go"));
        assert_eq!(report.halted().count(), 1);
        assert_eq!(report.summary().lines().count(), 2);
    }

    #[tokio::test]
    async fn redo_carries_the_original_task_and_none_of_the_failed_output() {
        let config = config();
        let invoker = ScriptedInvoker::new([("glm-go", vec![quota("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        let prompts = invoker.prompts();
        assert_eq!(prompts[0], "ORIGINAL TASK TEXT");
        let redo_prompt = &prompts[1];
        assert!(redo_prompt.contains("ORIGINAL TASK TEXT"));
        assert!(redo_prompt.contains("<redo_from_scratch>"));
        assert!(redo_prompt.contains("Rebuild from scratch: src/lib.rs."));
        assert!(redo_prompt.contains("Lane glm-go did not finish this task"));
        // Nothing the failed lane produced may reach the replacement.
        assert!(
            !redo_prompt.contains(PARTIAL),
            "failed attempt output leaked into the redo prompt"
        );
        assert!(!redo_prompt.contains("429"));

        let directive = run.attempts[1]
            .redo
            .as_ref()
            .expect("a lane change must issue a redo directive");
        assert_eq!(directive.from_lane, "glm-go");
        assert_eq!(directive.to_lane, "glm-cloud");
        assert_eq!(directive.artifacts, vec!["src/lib.rs".to_string()]);
    }

    #[tokio::test]
    async fn a_same_lane_retry_is_not_a_redo() {
        let config = config();
        let invoker = ScriptedInvoker::new([("glm-go", vec![transient("glm-go")])]);
        runner(&config, &invoker).run_task(&task()).await;

        let prompts = invoker.prompts();
        assert_eq!(prompts[0], "ORIGINAL TASK TEXT");
        // Same lane, so nothing was abandoned: the prompt is unchanged.
        assert_eq!(prompts[1], "ORIGINAL TASK TEXT");
    }

    #[tokio::test]
    async fn redo_is_suppressed_when_the_policy_says_resume() {
        let mut config = config();
        config.policy.redo_on_fallback = false;
        let invoker = ScriptedInvoker::new([("glm-go", vec![quota("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        assert_eq!(invoker.prompts()[1], "ORIGINAL TASK TEXT");
        assert!(run.attempts[1].redo.is_none());
    }

    #[tokio::test]
    async fn provenance_lists_assigned_versus_actual_and_can_be_suppressed() {
        let config = config();
        let invoker = ScriptedInvoker::new([("glm-go", vec![quota("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        assert_eq!(run.attempts.len(), 2);
        for attempt in &run.attempts {
            assert_eq!(attempt.assigned, "glm-go");
        }
        assert_eq!(run.attempts[0].actual, "glm-go");
        assert_eq!(run.attempts[1].actual, "glm-cloud");
        assert!(run.summary().contains("glm-go assigned, glm-cloud ran"));

        let mut quiet = config.clone();
        quiet.policy.record_provenance = false;
        let invoker = ScriptedInvoker::new([("glm-go", vec![quota("glm-go")])]);
        let quiet_run = runner(&quiet, &invoker).run_task(&task()).await;
        assert!(quiet_run.attempts.is_empty());
        // Suppressing the report must not blank out the failure trail the
        // aggregated error is built from.
        assert_eq!(quiet_run.trail.len(), 1);
        assert!(quiet_run.succeeded());
    }

    #[tokio::test]
    async fn max_retries_caps_the_whole_task_not_each_lane() {
        let mut config = config();
        config.policy.max_retries = 1;
        let invoker = ScriptedInvoker::new([
            ("glm-go", vec![transient("glm-go"), transient("glm-go")]),
            ("glm-cloud", vec![transient("glm-cloud")]),
            ("sonnet", vec![transient("sonnet")]),
        ]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        // Budget 1: only the first lane gets its retry, the rest advance at once.
        assert_eq!(
            invoker.lanes_called(),
            vec!["glm-go", "glm-go", "glm-cloud", "sonnet"]
        );
        assert_eq!(run.retries_used, 1);
        assert!(run.served.is_none());
        assert_eq!(run.trail.len(), 3);
    }

    #[tokio::test]
    async fn zero_retries_never_retries_a_lane() {
        let mut config = config();
        config.policy.max_retries = 0;
        let invoker = ScriptedInvoker::new([("glm-go", vec![transient("glm-go")])]);
        let run = runner(&config, &invoker).run_task(&task()).await;

        assert_eq!(invoker.lanes_called(), vec!["glm-go", "glm-cloud"]);
        assert_eq!(run.retries_used, 0);
    }

    #[test]
    fn a_roomier_sibling_is_tried_before_its_assigned_lane() {
        let config = config();
        // Both pools capped: the ordering is a real remaining fraction.
        let room = StaticRoom::new([
            pool(
                "opencode#opencode-go",
                "opencode",
                "opencode-go",
                900,
                Some(1_000),
            ),
            pool("opencode#ollama", "opencode", "ollama", 100, Some(1_000)),
        ]);
        let steps = resolve_lane(&config, &task(), &room).unwrap();

        assert_eq!(steps[0].lane, "glm-cloud");
        assert_eq!(steps[0].role, LaneRole::Sibling);
        assert_eq!(steps[0].assigned, "glm-go");
        assert_eq!(steps[1].lane, "glm-go");
        assert!(
            steps[0]
                .reason
                .contains("quota-derived (remaining fraction of a configured cap)")
        );
    }

    #[test]
    fn an_uncapped_pair_says_its_order_is_usage_derived_not_quota() {
        let config = config();
        let room = StaticRoom::new([
            pool("opencode#opencode-go", "opencode", "opencode-go", 900, None),
            pool("opencode#ollama", "opencode", "ollama", 100, None),
        ]);
        let steps = resolve_lane(&config, &task(), &room).unwrap();

        assert_eq!(steps[0].lane, "glm-cloud");
        assert!(
            steps[0]
                .reason
                .contains("usage-derived (observed 24h usage, not a quota measurement)"),
            "reason must state the basis verbatim: {}",
            steps[0].reason
        );
        assert!(!steps[0].reason.contains("quota-derived"));
    }

    #[test]
    fn a_shared_target_cap_is_reported_as_shared_never_divided() {
        let config = config();
        let shared = |key: &str, name: &str, used: u64| PoolHeadroom {
            tokens: PoolCap {
                scope: CapScope::Shared,
                limit: Some(1_000),
                used: 950,
                remaining: Some(50),
            },
            ..pool(key, "opencode", name, used, None)
        };
        let room = StaticRoom::new([
            shared("opencode#opencode-go", "opencode-go", 700),
            shared("opencode#ollama", "ollama", 250),
        ]);
        let steps = resolve_lane(&config, &task(), &room).unwrap();

        assert!(steps[0].reason.contains(SHARED_CAP_NOTE.trim()));
    }

    #[test]
    fn without_room_information_the_configured_order_stands() {
        let config = config();
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();

        assert_eq!(steps[0].lane, "glm-go");
        assert!(steps[0].reason.contains(NO_ROOM_NOTE));
    }

    #[test]
    fn sibling_crossover_can_be_switched_off() {
        let mut config = config();
        config.policy.prefer_sibling_on_quota = false;
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();

        let lanes: Vec<&str> = steps.iter().map(|step| step.lane.as_str()).collect();
        assert_eq!(lanes, vec!["glm-go", "sonnet"]);
    }

    #[test]
    fn an_implementation_task_skips_design_only_lanes_and_tiers() {
        let mut config = config();
        config.members.push(TeamMember {
            allow_impl: false,
            roles: vec!["plan".to_string()],
            ..TeamMember::new("opus", "claude", TeamMode::Cli)
        });
        config.members[0].fallback = vec!["opus".to_string(), "sonnet".to_string()];
        config.tiers = TierMap::from_pairs([("routine", vec!["glm-go"]), ("design", vec!["opus"])]);
        config.policy.design_only_tiers = Some(vec!["design".to_string()]);
        config.validate().unwrap();

        // The design-only fallback is dropped from an implementation walk.
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();
        let lanes: Vec<&str> = steps.iter().map(|step| step.lane.as_str()).collect();
        assert_eq!(lanes, vec!["glm-go", "glm-cloud", "sonnet"]);

        // The whole design tier refuses an implementation task outright.
        let refused =
            resolve_lane(&config, &task().with_tier("design"), &UNKNOWN_ROOM).unwrap_err();
        assert!(refused.to_string().contains("tier design is design-only"));

        // A design task reaches it.
        let planned = resolve_lane(
            &config,
            &LaneTask::design("plan", "think").with_tier("design"),
            &UNKNOWN_ROOM,
        )
        .unwrap();
        assert_eq!(planned[0].lane, "opus");
    }

    #[test]
    fn a_task_without_a_tier_starts_at_the_policy_default() {
        let mut config = config();
        config.tiers =
            TierMap::from_pairs([("mechanical", vec!["sonnet"]), ("routine", vec!["glm-go"])]);
        config.policy.default_tier = Some("routine".to_string());
        config.validate().unwrap();

        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();
        assert_eq!(steps[0].lane, "glm-go");
        assert!(steps[0].reason.starts_with("first lane of tier routine"));

        // With no configured default it is the ladder's first rung.
        config.policy.default_tier = None;
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();
        assert_eq!(steps[0].lane, "sonnet");
    }

    #[test]
    fn fallback_depth_is_bounded_by_the_policy() {
        let mut config = config();
        config.policy.max_fallback_depth = Some(0);
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();

        let lanes: Vec<&str> = steps.iter().map(|step| step.lane.as_str()).collect();
        assert_eq!(lanes, vec!["glm-go", "glm-cloud"]);
    }

    #[test]
    fn a_lane_is_never_walked_twice() {
        let mut config = config();
        config.tiers = TierMap::from_pairs([("routine", vec!["glm-go", "sonnet"])]);
        config.validate().unwrap();
        let steps = resolve_lane(&config, &task(), &UNKNOWN_ROOM).unwrap();

        let lanes: Vec<&str> = steps.iter().map(|step| step.lane.as_str()).collect();
        assert_eq!(lanes, vec!["glm-go", "glm-cloud", "sonnet"]);
    }

    #[test]
    fn redo_directive_has_no_channel_for_partial_output() {
        let directive = RedoDirective {
            from_lane: "glm-go".to_string(),
            to_lane: "glm-cloud".to_string(),
            artifacts: vec!["docs/x.md".to_string()],
        };
        let prompt = directive.apply("ORIGINAL");

        assert!(prompt.starts_with("ORIGINAL"));
        assert!(prompt.contains("Rebuild from scratch: docs/x.md."));
        assert!(prompt.contains("Do not resume, patch or append"));
    }

    #[tokio::test]
    async fn an_unresolvable_task_is_reported_not_dispatched() {
        let mut config = config();
        config.tiers = TierMap::from_pairs([("routine", vec!["glm-go"])]);
        let invoker = ScriptedInvoker::default();
        let run = runner(&config, &invoker)
            .run_task(&task().with_tier("nope"))
            .await;

        assert!(invoker.lanes_called().is_empty());
        assert!(run.unresolved.is_some());
        assert!(run.summary().contains("not dispatched"));
    }
}
