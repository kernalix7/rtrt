use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rtrt_core::{Config, CostClass, Error, Result, TeamConfig, TeamMember, TeamPolicy};
use rtrt_orchestrator::{
    NodeId, RecursionLimits, ResultProvenance, TeamNode, TeamTree, WorkerResult, WorkerReturn,
};
use serde::{Deserialize, Serialize};

use crate::{
    AgentInvoker, FailoverOutcome, FailurePolicy, InvocationContext, LaneRun, RankedTarget,
    lane::{LaneRunner, LaneTask, LedgerRoom, mode_from_team, resolve_leader_lane},
};

/// Successful dispatch plus the bounded root run retained for MCP integration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamDispatchResult {
    pub outcome: FailoverOutcome,
    pub run: LaneRun,
    pub recursion: RecursiveRunMetadata,
}

/// Runtime recursion projection and root snapshot. No child bridge is created
/// here: a parent coordinator must admit and dispatch every future child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveRunMetadata {
    pub enabled: bool,
    pub limits: RecursionLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<TeamNode>,
    pub node_count: u32,
}

/// Why lane provenance cannot be represented by the strict orchestrator DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceProjectionError {
    pub reason: String,
}

impl fmt::Display for ProvenanceProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for ProvenanceProjectionError {}

impl TeamDispatchResult {
    /// Convert the successful lane only when the strict assigned/actual model
    /// can express it. Full retry/failure detail remains in `run.attempts`.
    pub fn result_provenance(
        &self,
    ) -> std::result::Result<ResultProvenance, ProvenanceProjectionError> {
        let assigned = self
            .run
            .assigned
            .as_deref()
            .filter(|lane| !lane.trim().is_empty())
            .ok_or_else(|| projection_error("successful run has no assigned lane"))?;
        let actual = self
            .run
            .served_by
            .as_deref()
            .filter(|lane| !lane.trim().is_empty())
            .ok_or_else(|| projection_error("successful run has no actual lane"))?;
        if assigned == actual {
            return Ok(ResultProvenance::assigned(assigned));
        }

        let served_index = self
            .run
            .attempts
            .iter()
            .rposition(|attempt| attempt.class.is_none() && attempt.actual == actual)
            .ok_or_else(|| {
                projection_error("successful fallback lacks retained LaneAttempt provenance")
            })?;
        let served = &self.run.attempts[served_index];
        let from_lane = self.run.attempts[..served_index]
            .iter()
            .rev()
            .find(|attempt| attempt.class.is_some())
            .map(|attempt| attempt.actual.as_str())
            .ok_or_else(|| {
                projection_error(
                    "actual lane differs from assignment without a preceding failed lane",
                )
            })?;
        let provenance = ResultProvenance::fallback(
            assigned,
            actual,
            from_lane,
            served.reason.clone(),
            served.redo.is_some(),
        );
        provenance
            .validate()
            .map_err(|error| projection_error(error.to_string()))?;
        Ok(provenance)
    }

    /// Build strict result only from caller-supplied worker metadata. Provider
    /// output cannot safely invent files, tests, or a detail reference.
    pub fn worker_result(
        &self,
        output: WorkerReturn,
    ) -> std::result::Result<WorkerResult, ProvenanceProjectionError> {
        let result = WorkerResult {
            output,
            provenance: self.result_provenance()?,
        };
        result
            .validate()
            .map_err(|error| projection_error(error.to_string()))?;
        Ok(result)
    }
}

fn projection_error(reason: impl Into<String>) -> ProvenanceProjectionError {
    ProvenanceProjectionError {
        reason: reason.into(),
    }
}

/// Project already-validated config into orchestration runtime limits.
pub fn recursion_limits(policy: &TeamPolicy) -> Result<RecursionLimits> {
    policy.recursion.validate()?;
    let recursion = &policy.recursion;
    Ok(RecursionLimits {
        max_depth: recursion.effective_max_depth(),
        max_fan_out: recursion.effective_max_fan_out(),
        max_total_nodes: recursion.effective_max_total_nodes(),
        max_tokens: recursion.effective_max_tokens(),
        deadline_secs: recursion.effective_deadline_secs(),
        max_payload_bytes: recursion.effective_max_payload_bytes(),
        subleader_tiers: if recursion.enabled {
            recursion.subleader_tiers.clone()
        } else {
            Vec::new()
        },
    })
}

/// Dispatch a task to the first available configured team leader.
///
/// The leader candidates are walked by the lane runner rather than by a bare
/// failover list, so the leader gets the same treatment every delegated task
/// gets: a quota wall crosses to the leader's sibling pool before it downgrades
/// the model, a transient hiccup earns one same-lane retry inside the task's
/// `max_retries` budget, and a fatal failure halts instead of replaying a broken
/// credential against every remaining leader. A roster that declares no
/// `sibling` and no `fallback` collapses to `leader_order` itself, which is
/// exactly what this walked before lanes existed.
pub async fn dispatch_team(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
) -> Result<FailoverOutcome> {
    Ok(dispatch_team_rich(config, prompt, timeout).await?.outcome)
}

pub async fn dispatch_team_with_context(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
    provenance: Option<InvocationContext>,
) -> Result<FailoverOutcome> {
    Ok(
        dispatch_team_rich_with_context(config, prompt, timeout, provenance)
            .await?
            .outcome,
    )
}

pub async fn dispatch_team_rich(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
) -> Result<TeamDispatchResult> {
    dispatch_team_rich_with_context(config, prompt, timeout, None).await
}

pub async fn dispatch_team_rich_with_context(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
    provenance: Option<InvocationContext>,
) -> Result<TeamDispatchResult> {
    if prompt.trim().is_empty() {
        return Err(Error::Provider(
            "team dispatch prompt must not be empty".to_string(),
        ));
    }
    // Validates the roster and keeps the pre-existing error messages for a
    // disabled config or an unknown leader.
    ranked_leaders(config)?;
    let leader_prompt = build_team_leader_prompt(config, prompt);
    let recursion = root_metadata(config, leader_prompt.len())?;
    let effective = Config::load_effective_for_cwd();
    let failure = FailurePolicy::from_config(&effective.failover);
    let room = LedgerRoom::new(effective);
    let steps = resolve_leader_lane(config, &room)?;

    let invoker = AgentInvoker::with_provenance(provenance);
    let run = LaneRunner::new(config, &invoker)
        .with_room(&room)
        .with_failure_policy(failure)
        .with_timeout(timeout)
        .run_steps(&leader_task(&leader_prompt), &steps)
        .await;
    if let Some(reason) = run.unresolved {
        return Err(Error::Config(reason));
    }
    let outcome = run.clone().into_policy_outcome().into_failover()?;
    Ok(TeamDispatchResult {
        outcome,
        run,
        recursion,
    })
}

fn root_metadata(config: &TeamConfig, payload_len: usize) -> Result<RecursiveRunMetadata> {
    let limits = recursion_limits(&config.policy)?;
    if !config.policy.recursion.enabled {
        return Ok(RecursiveRunMetadata {
            enabled: false,
            limits,
            root: None,
            node_count: 0,
        });
    }
    if payload_len > limits.max_payload_bytes as usize {
        return Err(Error::Config(format!(
            "team recursive root payload is {payload_len} bytes, exceeding max_payload_bytes {}",
            limits.max_payload_bytes
        )));
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let tree = TeamTree::new(limits.clone(), now_ms)
        .map_err(|error| Error::Config(format!("team recursive root admission failed: {error}")))?;
    let root = tree.node(NodeId::ROOT).cloned().ok_or_else(|| {
        Error::Config("team recursive root admission produced no root node".to_string())
    })?;
    let node_count = u32::try_from(tree.nodes().count()).unwrap_or(u32::MAX);
    Ok(RecursiveRunMetadata {
        enabled: true,
        limits,
        root: Some(root),
        node_count,
    })
}

/// The leader's task.
///
/// `implements` is false because a leader plans, delegates and integrates — the
/// shipped roster's first leader is design-only, and filtering it out would
/// change who leads. Redo directives are off because
/// [`build_team_leader_prompt`] already restates the whole original task and
/// carries no partial output, so re-sending it to the next leader IS a
/// from-scratch redo; adding a directive would only perturb bytes an existing
/// deployment depends on.
fn leader_task(leader_prompt: &str) -> LaneTask {
    LaneTask::design("team-leader", leader_prompt).without_redo()
}

/// Build the shared instructions passed unchanged to every fallback leader.
///
/// The routing and failure sections are *rendered from the config*, not
/// written here: which lane serves which difficulty, how a quota wall is
/// crossed, and how deep a fallback walk goes are all user data, so editing
/// `[team.tiers]` / `[team.policy]` changes what the leader is told.
pub fn build_team_leader_prompt(config: &TeamConfig, prompt: &str) -> String {
    let roster = config
        .members
        .iter()
        .map(format_member)
        .collect::<Vec<_>>()
        .join("\n");
    let routing = format_routing(config);
    let policy = format_policy(config);
    let delegation = format_delegation(config);

    format!(
        "You are selected available team leader.\n\
         Analyze the user task, retain responsibility for architecture and integration, and split independent work into parallel tasks.\n\
         {delegation}\
         {routing}\
         Do not assign work back to the member matching your own target and model when doing so would recurse.\n\
         Review all delegated results, resolve conflicts, integrate the work, and verify the final result.\n\
         \n\
         Full team roster:\n{roster}\n\
         \n\
         Failure and fallback policy:\n{policy}\n\
         \n\
         <original_user_task>\n{prompt}\n</original_user_task>"
    )
}

fn format_delegation(config: &TeamConfig) -> String {
    if !config.policy.recursion.enabled {
        return "Delegate through the rtrt MCP agent_call tool, calling independent members in parallel with each member's target and model.\n".to_string();
    }
    let recursion = &config.policy.recursion;
    format!(
        "The parent remains coordinator and exclusively owns recursive admission, dispatch, integration, and cancellation.\n\
         Do not invoke OpenCode Task, rtrt agent_call/team_dispatch, or contact sibling/child agents directly; return proposed child tasks to the parent coordinator.\n\
         Recursive proposals are bounded to depth {}, fan-out {}, total nodes {}, {} tokens, {} seconds, and {} payload bytes; only these tiers may sublead: {}.\n",
        recursion.max_depth,
        recursion.max_fan_out,
        recursion.max_total_nodes,
        recursion.max_tokens,
        recursion.deadline_secs,
        recursion.max_payload_bytes,
        if recursion.subleader_tiers.is_empty() {
            "<none>".to_string()
        } else {
            recursion.subleader_tiers.join(", ")
        }
    )
}

/// The configured difficulty ladder, as instructions. Empty when the config
/// declares no tiers at all — better to say nothing than to invent a heuristic.
fn format_routing(config: &TeamConfig) -> String {
    let tiers = config.effective_tiers();
    if tiers.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "Route by task difficulty using the tiers below; inside a tier the first member is preferred and the rest are its alternates.\n",
    );
    if let Some(default_tier) = config.effective_default_tier() {
        out.push_str(&format!(
            "When a task's difficulty is unclear, start at the {default_tier} tier.\n"
        ));
    }
    for (tier, members) in tiers.iter() {
        let note = if config.is_design_only_tier(tier) {
            " (design only: plan and review, do not implement)"
        } else {
            ""
        };
        out.push_str(&format!("- tier {tier}{note}: {}\n", members.join(" > ")));
    }
    out
}

/// The configured failure policy, as instructions.
fn format_policy(config: &TeamConfig) -> String {
    let policy = &config.policy;
    let mut lines = vec![match policy.max_retries {
        0 => "- Do not retry a failed member: act on the first failure.".to_string(),
        1 => "- Retry a transient failure (timeout, network, 5xx) once on the same member."
            .to_string(),
        retries => format!(
            "- Retry a transient failure (timeout, network, 5xx) up to {retries} times on the same member."
        ),
    }];
    lines.push(
        "- A quota or rate-limit refusal is not transient: never retry the same member on it."
            .to_string(),
    );
    lines.push(
        if policy.prefer_sibling_on_quota {
            "- On a quota refusal, move the work to that member's sibling first (same logical model, different pool), and only then walk its fallback chain."
        } else {
            "- On a quota refusal, walk that member's fallback chain; do not cross over to its sibling pool."
        }
        .to_string(),
    );
    lines.push(
        "- A fatal failure (bad request, missing credentials, unknown model) ends the walk: report it instead of falling back."
            .to_string(),
    );
    lines.push(format!(
        "- Walk at most {} member(s) of a fallback chain before reporting the task as failed.",
        config.effective_max_fallback_depth()
    ));
    lines.push(
        if policy.redo_on_fallback {
            "- After falling back, redo the delegated work from scratch on the replacement member; do not reuse the failed member's partial output."
        } else {
            "- After falling back, resume from the failed member's partial output instead of redoing the work."
        }
        .to_string(),
    );
    if policy.record_provenance {
        lines.push("- Report which member produced each delegated result.".to_string());
    }
    lines.join("\n")
}

fn ranked_leaders(config: &TeamConfig) -> Result<Vec<RankedTarget>> {
    if !config.enabled {
        return Err(Error::Config(
            "team must be enabled before dispatch".to_string(),
        ));
    }
    config.validate()?;

    config
        .leader_order
        .iter()
        .map(|leader_name| {
            let member = config
                .members
                .iter()
                .find(|member| member.name == *leader_name)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "team leader references unknown member: {leader_name}"
                    ))
                })?;
            Ok(RankedTarget {
                target: member.target.clone(),
                mode: mode_from_team(member.mode),
                model: member.model.clone(),
                cost_class: CostClass::Unknown,
            })
        })
        .collect()
}

fn format_member(member: &TeamMember) -> String {
    let mut line = format!(
        "- name: {}; target: {}; model: {}; roles: {}",
        member.name,
        member.target,
        member.model.as_deref().unwrap_or("<default>"),
        member.roles.join(", ")
    );
    if let Some(logical) = &member.logical {
        line.push_str(&format!("; logical: {logical}"));
    }
    if let Some(tier) = &member.tier {
        line.push_str(&format!("; tier: {tier}"));
    }
    if let Some(sibling) = &member.sibling {
        line.push_str(&format!("; sibling: {sibling}"));
    }
    if !member.fallback.is_empty() {
        line.push_str(&format!("; fallback: {}", member.fallback.join(" > ")));
    }
    if !member.allow_impl {
        line.push_str("; design only (no implementation)");
    }
    if !member.flags.is_empty() {
        let flags = member
            .flags
            .iter()
            .map(|(key, value)| {
                if value.is_empty() {
                    key.clone()
                } else {
                    format!("{key}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!("; flags: {flags}"));
    }
    line
}

#[cfg(test)]
mod tests {
    use rtrt_core::{TeamMode, TierMap};

    use super::*;
    use crate::{
        Mode,
        lane::{LaneRole, UNKNOWN_ROOM},
    };

    fn member(
        name: &str,
        target: &str,
        model: Option<&str>,
        mode: TeamMode,
        roles: &[&str],
    ) -> TeamMember {
        TeamMember {
            model: model.map(str::to_string),
            roles: roles.iter().map(|role| (*role).to_string()).collect(),
            ..TeamMember::new(name, target, mode)
        }
    }

    fn config() -> TeamConfig {
        TeamConfig {
            enabled: true,
            manager_provider: "local".to_string(),
            manager_model: "manager".to_string(),
            manager_base_url: None,
            leader_order: vec!["second".to_string(), "first".to_string()],
            members: vec![
                member(
                    "first",
                    "claude",
                    Some("sonnet"),
                    TeamMode::Cli,
                    &["tests", "review"],
                ),
                member(
                    "second",
                    "opencode",
                    Some("openai/gpt-5.6-sol"),
                    TeamMode::Api,
                    &["lead", "debugging"],
                ),
                member(
                    "worker",
                    "ollama",
                    None,
                    TeamMode::Auto,
                    &["routine", "bulk-edit"],
                ),
            ],
            ..TeamConfig::default()
        }
    }

    fn recursive_config() -> TeamConfig {
        let mut config = config();
        config.tiers = TierMap::from_pairs([("lead", vec!["second"])]);
        config.policy.recursion.enabled = true;
        config.policy.recursion.max_depth = 2;
        config.policy.recursion.max_fan_out = 2;
        config.policy.recursion.max_total_nodes = 5;
        config.policy.recursion.max_tokens = 10_000;
        config.policy.recursion.deadline_secs = 60;
        config.policy.recursion.max_payload_bytes = 16_384;
        config.policy.recursion.subleader_tiers = vec!["lead".to_string()];
        config
    }

    #[test]
    fn leaders_follow_exact_configured_order() {
        let leaders = ranked_leaders(&config()).unwrap();

        assert_eq!(
            leaders,
            vec![
                RankedTarget {
                    target: "opencode".to_string(),
                    mode: Mode::Api,
                    model: Some("openai/gpt-5.6-sol".to_string()),
                    cost_class: CostClass::Unknown,
                },
                RankedTarget {
                    target: "claude".to_string(),
                    mode: Mode::Cli,
                    model: Some("sonnet".to_string()),
                    cost_class: CostClass::Unknown,
                },
            ]
        );
    }

    #[test]
    fn prompt_contains_full_roster_and_roles() {
        let prompt = build_team_leader_prompt(&config(), "task");

        assert!(
            prompt.contains("name: first; target: claude; model: sonnet; roles: tests, review")
        );
        assert!(prompt.contains(
            "name: second; target: opencode; model: openai/gpt-5.6-sol; roles: lead, debugging"
        ));
        assert!(
            prompt.contains(
                "name: worker; target: ollama; model: <default>; roles: routine, bulk-edit"
            )
        );
    }

    #[test]
    fn recursive_policy_projects_to_a_bounded_root_without_child_permissions() {
        let config = recursive_config();
        config.validate().unwrap();
        let prompt = build_team_leader_prompt(&config, "task");
        let metadata = root_metadata(&config, prompt.len()).unwrap();

        assert!(metadata.enabled);
        assert_eq!(metadata.node_count, 1);
        assert_eq!(metadata.limits.max_depth, 2);
        assert_eq!(metadata.limits.max_fan_out, 2);
        assert_eq!(metadata.limits.max_total_nodes, 5);
        assert_eq!(metadata.limits.subleader_tiers, vec!["lead"]);
        let root = metadata.root.unwrap();
        assert_eq!(root.id, NodeId::ROOT);
        assert_eq!(root.parent, None);
        assert!(root.children.is_empty());
        assert_eq!(root.grant.nodes, 4);
        assert!(prompt.contains("The parent remains coordinator"));
        assert!(prompt.contains("Do not invoke OpenCode Task, rtrt agent_call/team_dispatch"));
        assert!(!prompt.contains("Delegate through the rtrt MCP agent_call tool"));
    }

    #[test]
    fn recursive_root_rejects_payload_above_projected_limit() {
        let mut config = recursive_config();
        config.policy.recursion.max_payload_bytes = 1;
        let error = root_metadata(&config, 2).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("team recursive root payload is 2 bytes, exceeding max_payload_bytes 1")
        );
    }

    #[test]
    fn rich_result_projects_real_fallback_provenance_without_worker_fictions() {
        let outcome = crate::InvokeOutcome {
            target: "claude".to_string(),
            mode_used: Mode::Cli,
            model: Some("sonnet".to_string()),
            output: "done".to_string(),
            cost_usd: None,
            exit_code: Some(0),
            ms: 1,
        };
        let failed = crate::LaneAttempt {
            assigned: "second".to_string(),
            actual: "second".to_string(),
            target: "opencode".to_string(),
            model: Some("openai/gpt-5.6-sol".to_string()),
            role: crate::LaneRole::Primary,
            reason: "first configured leader".to_string(),
            class: Some(crate::FailureClass::Transient),
            retried: true,
            redo: None,
            error: Some("provider timed out".to_string()),
        };
        let served = crate::LaneAttempt {
            assigned: "second".to_string(),
            actual: "first".to_string(),
            target: "claude".to_string(),
            model: Some("sonnet".to_string()),
            role: crate::LaneRole::Fallback,
            reason: "fallback 1 of second".to_string(),
            class: None,
            retried: false,
            redo: Some(crate::RedoDirective {
                from_lane: "second".to_string(),
                to_lane: "first".to_string(),
                artifacts: Vec::new(),
            }),
            error: None,
        };
        let result = TeamDispatchResult {
            outcome: FailoverOutcome {
                outcome: outcome.clone(),
                failed_over: Vec::new(),
            },
            run: LaneRun {
                task_id: "team-leader".to_string(),
                assigned: Some("second".to_string()),
                served_by: Some("first".to_string()),
                served: Some(outcome),
                attempts: vec![failed, served],
                trail: Vec::new(),
                halted: None,
                retries_used: 1,
                unresolved: None,
            },
            recursion: RecursiveRunMetadata {
                enabled: false,
                limits: recursion_limits(&TeamPolicy::default()).unwrap(),
                root: None,
                node_count: 0,
            },
        };

        let provenance = result.result_provenance().unwrap();
        assert_eq!(provenance.assigned_lane, "second");
        assert_eq!(provenance.actual_lane, "first");
        let fallback = provenance.fallback.unwrap();
        assert_eq!(fallback.from_lane, "second");
        assert_eq!(fallback.reason, "fallback 1 of second");
        assert!(fallback.redo_from_scratch);
    }

    #[test]
    fn original_prompt_is_byte_preserved_between_delimiters() {
        let original = "  first line\n\nUTF-8: 한글\r\n<xml>& bytes  ";
        let prompt = build_team_leader_prompt(&config(), original);
        let preserved = prompt
            .strip_prefix(prompt.split_once("<original_user_task>\n").unwrap().0)
            .unwrap()
            .strip_prefix("<original_user_task>\n")
            .unwrap()
            .strip_suffix("\n</original_user_task>")
            .unwrap();

        assert_eq!(preserved.as_bytes(), original.as_bytes());
    }

    #[test]
    fn disabled_config_is_rejected() {
        let mut config = config();
        config.enabled = false;

        let error = ranked_leaders(&config).unwrap_err();
        assert_eq!(
            error.to_string(),
            "config error: team must be enabled before dispatch"
        );
    }

    #[tokio::test]
    async fn empty_prompt_is_rejected_before_invocation() {
        let error = dispatch_team(&config(), " \n ", Duration::from_secs(1))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "provider error: team dispatch prompt must not be empty"
        );
    }

    #[test]
    fn routing_instructions_come_from_the_configured_tiers() {
        // No tiers configured for this roster: the prompt states no routing
        // heuristic at all rather than inventing one.
        let untiered = build_team_leader_prompt(&config(), "task");
        assert!(!untiered.contains("Route by task difficulty"));

        let mut tiered = config();
        tiered.tiers = TierMap::from_pairs([
            ("mechanical", vec!["worker"]),
            ("hard", vec!["second", "first"]),
            ("plan", vec!["first"]),
        ]);
        tiered.policy.design_only_tiers = Some(vec!["plan".to_string()]);
        let prompt = build_team_leader_prompt(&tiered, "task");

        assert!(prompt.contains(
            "Route by task difficulty using the tiers below; inside a tier the first member is \
             preferred and the rest are its alternates."
        ));
        assert!(
            prompt.contains("When a task's difficulty is unclear, start at the mechanical tier.")
        );
        assert!(prompt.contains("- tier mechanical: worker"));
        assert!(prompt.contains("- tier hard: second > first"));
        assert!(
            prompt.contains("- tier plan (design only: plan and review, do not implement): first")
        );
        assert_ne!(prompt, untiered);

        // Editing the config edits the instruction — the whole point.
        let mut relabelled = tiered.clone();
        relabelled.tiers = TierMap::from_pairs([("bulk", vec!["first", "worker"])]);
        relabelled.policy.design_only_tiers = None;
        let relabelled_prompt = build_team_leader_prompt(&relabelled, "task");
        assert!(relabelled_prompt.contains("- tier bulk: first > worker"));
        assert!(!relabelled_prompt.contains("mechanical"));
        assert_ne!(relabelled_prompt, prompt);
    }

    #[test]
    fn model_family_heuristics_are_no_longer_hardcoded() {
        // The routing prose used to name GLM / Kimi / GPT / Sonnet directly, so
        // a roster without those models still got their instructions.
        let prompt = build_team_leader_prompt(&config(), "task");
        for hardcoded in [
            "Prefer GLM or Kimi members",
            "GPT members for hard implementation",
            "Sonnet members for general implementation",
        ] {
            assert!(!prompt.contains(hardcoded), "still hardcoded: {hardcoded}");
        }
    }

    #[test]
    fn failure_policy_is_rendered_from_config() {
        let prompt = build_team_leader_prompt(&config(), "task");
        assert!(prompt.contains(
            "- Retry a transient failure (timeout, network, 5xx) up to 2 times on the same member."
        ));
        assert!(prompt.contains(
            "- On a quota refusal, move the work to that member's sibling first (same logical \
             model, different pool), and only then walk its fallback chain."
        ));
        assert!(prompt.contains("- Walk at most 3 member(s) of a fallback chain"));
        assert!(prompt.contains("- After falling back, redo the delegated work from scratch"));
        assert!(prompt.contains("- Report which member produced each delegated result."));

        let mut strict = config();
        strict.policy.max_retries = 0;
        strict.policy.prefer_sibling_on_quota = false;
        strict.policy.redo_on_fallback = false;
        strict.policy.record_provenance = false;
        strict.policy.max_fallback_depth = Some(1);
        let prompt = build_team_leader_prompt(&strict, "task");
        assert!(prompt.contains("- Do not retry a failed member: act on the first failure."));
        assert!(prompt.contains("do not cross over to its sibling pool."));
        assert!(prompt.contains("- Walk at most 1 member(s) of a fallback chain"));
        assert!(
            prompt.contains("- After falling back, resume from the failed member's partial output")
        );
        assert!(!prompt.contains("- Report which member produced each delegated result."));
    }

    #[test]
    fn roster_lines_carry_the_lane_wiring() {
        let mut wired = config();
        wired.members[0].logical = Some("sonnet".to_string());
        wired.members[0].tier = Some("review".to_string());
        wired.members[0].allow_impl = false;
        wired.members[0].fallback = vec!["second".to_string(), "worker".to_string()];
        wired.members[1].logical = Some("gpt-5.6-sol".to_string());
        wired.members[1].sibling = Some("worker".to_string());
        wired.members[2].logical = Some("gpt-5.6-sol".to_string());
        wired.members[2]
            .flags
            .insert("permission-mode".to_string(), "acceptEdits".to_string());

        let prompt = build_team_leader_prompt(&wired, "task");
        assert!(prompt.contains(
            "- name: first; target: claude; model: sonnet; roles: tests, review; logical: sonnet; \
             tier: review; fallback: second > worker; design only (no implementation)"
        ));
        assert!(prompt.contains(
            "- name: second; target: opencode; model: openai/gpt-5.6-sol; roles: lead, debugging; \
             logical: gpt-5.6-sol; sibling: worker"
        ));
        assert!(prompt.contains(
            "- name: worker; target: ollama; model: <default>; roles: routine, bulk-edit; \
             logical: gpt-5.6-sol; flags: permission-mode=acceptEdits"
        ));
        // A member's own tier declaration reaches the routing block too.
        assert!(prompt.contains("- tier review"));
    }

    #[test]
    fn a_legacy_roster_walks_exactly_the_configured_leader_order() {
        // No `sibling`, no `fallback` anywhere: the lane expansion must collapse
        // to `leader_order` itself, which is what dispatch walked before lanes
        // existed. Same order, same targets, same models, no extra candidates.
        let config = config();
        let steps = resolve_leader_lane(&config, &UNKNOWN_ROOM).unwrap();

        assert_eq!(
            steps
                .iter()
                .map(|step| (
                    step.lane.as_str(),
                    step.target.as_str(),
                    step.model.as_deref(),
                    step.mode
                ))
                .collect::<Vec<_>>(),
            vec![
                ("second", "opencode", Some("openai/gpt-5.6-sol"), Mode::Api),
                ("first", "claude", Some("sonnet"), Mode::Cli),
            ]
        );
        assert_eq!(
            steps
                .iter()
                .map(|step| step.ranked_target())
                .collect::<Vec<_>>(),
            ranked_leaders(&config).unwrap()
        );
        assert_eq!(steps[0].role, LaneRole::Primary);
        assert_eq!(steps[1].role, LaneRole::Alternate);
    }

    #[test]
    fn a_leader_with_lane_wiring_gains_its_sibling_and_fallback() {
        let mut config = config();
        config.members[1].logical = Some("gpt-5.6-sol".to_string());
        config.members[1].sibling = Some("worker".to_string());
        config.members[1].fallback = vec!["first".to_string()];
        config.members[2].logical = Some("gpt-5.6-sol".to_string());
        config.members[2].sibling = Some("second".to_string());
        config.validate().unwrap();

        let steps = resolve_leader_lane(&config, &UNKNOWN_ROOM).unwrap();
        assert_eq!(
            steps
                .iter()
                .map(|step| (step.lane.as_str(), step.role, step.assigned.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("second", LaneRole::Primary, "second"),
                ("worker", LaneRole::Sibling, "second"),
                ("first", LaneRole::Fallback, "second"),
            ]
        );
    }

    #[test]
    fn a_design_only_leader_is_never_filtered_out_of_the_leader_walk() {
        // The shipped roster's first leader plans rather than implements; a walk
        // that dropped it would silently change who leads.
        let mut config = config();
        config.members[1].allow_impl = false;
        config.tiers = TierMap::from_pairs([("design", vec!["second"])]);
        config.policy.design_only_tiers = Some(vec!["design".to_string()]);
        config.validate().unwrap();

        let steps = resolve_leader_lane(&config, &UNKNOWN_ROOM).unwrap();
        assert_eq!(steps[0].lane, "second");
    }

    #[test]
    fn unknown_leader_is_rejected() {
        let mut config = config();
        config.leader_order = vec!["missing".to_string()];

        let error = ranked_leaders(&config).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("references unknown member: missing")
        );
    }
}
