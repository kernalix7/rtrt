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
pub fn build_team_leader_prompt(config: &TeamConfig, prompt: &str) -> String {
    let roster = config
        .members
        .iter()
        .map(format_member)
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are selected available team leader.\n\
         Analyze the user task, retain responsibility for architecture and integration, and split independent work into parallel tasks.\n\
         Delegate through the rtrt MCP agent_call tool, calling independent members in parallel with each member's target and model.\n\
         Prefer GLM or Kimi members for routine work, GPT members for hard implementation or debugging, and Sonnet members for general implementation, tests, and review.\n\
         Do not assign work back to the member matching your own target and model when doing so would recurse.\n\
         Review all delegated results, resolve conflicts, integrate the work, and verify the final result.\n\
         \n\
         Full team roster:\n{roster}\n\
         \n\
         <original_user_task>\n{prompt}\n</original_user_task>"
    )
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
    format!(
        "- name: {}; target: {}; model: {}; roles: {}",
        member.name,
        member.target,
        member.model.as_deref().unwrap_or("<default>"),
        member.roles.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(
        name: &str,
        target: &str,
        model: Option<&str>,
        mode: TeamMode,
        roles: &[&str],
    ) -> TeamMember {
        TeamMember {
            name: name.to_string(),
            target: target.to_string(),
            model: model.map(str::to_string),
            mode,
            roles: roles.iter().map(|role| (*role).to_string()).collect(),
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
