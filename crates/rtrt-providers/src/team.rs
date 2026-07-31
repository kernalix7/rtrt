use std::time::Duration;

use rtrt_core::{CostClass, Error, Result, TeamConfig, TeamMember, TeamMode};

use crate::{FailoverOutcome, Mode, RankedTarget, invoke_with_failover};

/// Dispatch a task to the first available configured team leader.
pub async fn dispatch_team(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
) -> Result<FailoverOutcome> {
    if prompt.trim().is_empty() {
        return Err(Error::Provider(
            "team dispatch prompt must not be empty".to_string(),
        ));
    }
    let leaders = ranked_leaders(config)?;
    let leader_prompt = build_team_leader_prompt(config, prompt);
    invoke_with_failover(&leaders, &leader_prompt, timeout).await
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

    format!(
        "You are selected available team leader.\n\
         Analyze the user task, retain responsibility for architecture and integration, and split independent work into parallel tasks.\n\
         Delegate through the rtrt MCP agent_call tool, calling independent members in parallel with each member's target and model.\n\
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

fn mode_from_team(mode: TeamMode) -> Mode {
    match mode {
        TeamMode::Cli => Mode::Cli,
        TeamMode::Api => Mode::Api,
        TeamMode::Auto => Mode::Auto,
    }
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
    use rtrt_core::TierMap;

    use super::*;

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
