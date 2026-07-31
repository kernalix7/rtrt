use std::{pin::Pin, process::Stdio, time::Duration};

use futures_util::{Stream, stream};
use rtrt_core::{CostClass, Error, Result, TeamConfig, TeamMember, TeamMode};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::{
    FailoverOutcome, InvokeOptions, Mode, RankedTarget, invoke_agent, invoke_with_failover,
    is_retryable_error, template_to_argv, usage::UsageSnapshot, usage_ledger,
};

#[cfg(unix)]
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

/// Text-only leader stream. Transport metadata and tool JSON never enter the
/// caller's model context; only assistant text deltas cross this boundary.
pub type TeamTextStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

#[derive(Debug, Clone, Default)]
pub struct TeamExecutionContext {
    pub opencode_session_id: Option<String>,
    pub opencode_server_url: Option<String>,
}

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
    let leaders = ranked_leaders(config, &UsageSnapshot::load_for_routing())?;
    let leader_prompt = build_team_leader_prompt(config, prompt);
    invoke_with_failover(&leaders, &leader_prompt, timeout).await
}

/// Dispatch through configured leaders while streaming assistant text.
///
/// CLI leaders are streamed directly. Claude's `stream-json` and OpenCode's
/// JSON event feed are parsed internally; UUIDs, usage blocks, tool arguments,
/// and other transport metadata are discarded. A leader that fails before
/// emitting text falls through to the next configured leader. Once text has
/// been emitted, failover stops because already-visible output cannot be
/// retracted safely.
pub fn dispatch_team_stream(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
) -> Result<TeamTextStream> {
    dispatch_team_stream_with_context(config, prompt, timeout, TeamExecutionContext::default())
}

pub fn dispatch_team_stream_with_context(
    config: &TeamConfig,
    prompt: &str,
    timeout: Duration,
    context: TeamExecutionContext,
) -> Result<TeamTextStream> {
    if prompt.trim().is_empty() {
        return Err(Error::Provider(
            "team dispatch prompt must not be empty".to_string(),
        ));
    }
    let leaders = ranked_leaders(config, &UsageSnapshot::load_for_routing())?;
    let leader_prompt = build_team_leader_prompt(config, prompt);
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        stream_leader_failover(leaders, leader_prompt, timeout, context, tx).await;
    });
    Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })))
}

async fn stream_leader_failover(
    leaders: Vec<RankedTarget>,
    prompt: String,
    timeout: Duration,
    context: TeamExecutionContext,
    tx: mpsc::Sender<Result<String>>,
) {
    let started = std::time::Instant::now();
    let mut failures = Vec::new();
    for leader in leaders {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            failures.push("leader routing deadline exhausted".to_string());
            break;
        }
        if tx
            .send(Ok(format!(
                "[rtrt] Leader: {}\n",
                leader_display_name(&leader)
            )))
            .await
            .is_err()
        {
            return;
        }
        let result = stream_one_leader(&leader, &prompt, remaining, &context, &tx).await;
        match result {
            Ok(()) => return,
            Err(StreamLeaderError { error, emitted }) => {
                usage_ledger::record_invocation(
                    &leader_usage_target(&leader.target, leader.model.as_deref()),
                    leader.model.as_deref().unwrap_or_default(),
                    usage_ledger::estimate_tokens(&prompt),
                    0,
                    true,
                    false,
                );
                let retryable = is_retryable_error(&error);
                failures.push(format!(
                    "{}[{}]: {}",
                    leader.target,
                    leader.model.as_deref().unwrap_or("default"),
                    error
                ));
                if emitted || !retryable {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
        }
    }
    let _ = tx
        .send(Err(Error::Provider(format!(
            "team streaming failed: {}",
            failures.join("; ")
        ))))
        .await;
}

#[derive(Debug)]
struct StreamLeaderError {
    error: Error,
    emitted: bool,
}

async fn stream_one_leader(
    leader: &RankedTarget,
    prompt: &str,
    timeout: Duration,
    context: &TeamExecutionContext,
    tx: &mpsc::Sender<Result<String>>,
) -> std::result::Result<(), StreamLeaderError> {
    if leader.mode != Mode::Cli {
        let result = invoke_agent(
            &leader.target,
            prompt,
            InvokeOptions {
                mode: Some(leader.mode),
                model: leader.model.clone(),
                timeout,
            },
        )
        .await;
        return match result {
            Ok(outcome) => tx
                .send(Ok(outcome.output))
                .await
                .map_err(|_| StreamLeaderError {
                    error: Error::Provider("team stream receiver closed".to_string()),
                    emitted: false,
                }),
            Err(error) => Err(StreamLeaderError {
                error,
                emitted: false,
            }),
        };
    }

    let tools = tokio::task::spawn_blocking(rtrt_core::detect_tools)
        .await
        .map_err(|e| StreamLeaderError {
            error: Error::Provider(format!("team stream tool detection failed: {e}")),
            emitted: false,
        })?;
    let tool = tools
        .iter()
        .find(|tool| tool.name == leader.target)
        .filter(|tool| tool.installed && tool.enabled)
        .ok_or_else(|| StreamLeaderError {
            error: Error::Provider(format!(
                "team stream target '{}' is not installed or enabled",
                leader.target
            )),
            emitted: false,
        })?;
    let template = tool
        .cli_invocation
        .as_deref()
        .ok_or_else(|| StreamLeaderError {
            error: Error::Provider(format!(
                "team stream target '{}' has no CLI invocation",
                leader.target
            )),
            emitted: false,
        })?;
    let mut argv =
        template_to_argv(template, prompt, leader.model.as_deref()).map_err(|error| {
            StreamLeaderError {
                error,
                emitted: false,
            }
        })?;
    remove_prompt_argument(&mut argv, prompt);
    enable_json_stream(&leader.target, &mut argv);
    run_streaming_cli(
        &leader.target,
        leader.model.as_deref(),
        prompt,
        &argv,
        timeout,
        context,
        tx,
    )
    .await
}

fn remove_prompt_argument(argv: &mut Vec<String>, prompt: &str) {
    if let Some(index) = argv.iter().position(|argument| argument == prompt) {
        argv.remove(index);
    }
}

fn enable_json_stream(target: &str, argv: &mut Vec<String>) {
    match target {
        "claude" => argv.extend([
            "--verbose".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--include-partial-messages".to_string(),
        ]),
        "opencode" => {
            if let Some(index) = argv.iter().position(|arg| arg == "build")
                && index > 0
                && argv[index - 1] == "--agent"
            {
                argv[index] = "rtrt-leader".to_string();
            }
            argv.extend(["--format".to_string(), "json".to_string()]);
        }
        _ => {}
    }
}

async fn run_streaming_cli(
    target: &str,
    model: Option<&str>,
    prompt: &str,
    argv: &[String],
    timeout: Duration,
    context: &TeamExecutionContext,
    tx: &mpsc::Sender<Result<String>>,
) -> std::result::Result<(), StreamLeaderError> {
    let Some(program) = argv.first() else {
        return Err(StreamLeaderError {
            error: Error::Provider("team stream command is empty".to_string()),
            emitted: false,
        });
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(session_id) = &context.opencode_session_id {
        command.env("RTRT_OPENCODE_PARENT_SESSION", session_id);
    }
    if let Some(server_url) = &context.opencode_server_url {
        command.env("RTRT_OPENCODE_SERVER_URL", server_url);
    }
    #[cfg(unix)]
    command.process_group(0);
    let child = command.spawn().map_err(|e| StreamLeaderError {
        error: Error::Provider(format!("team stream spawn {program}: {e}")),
        emitted: false,
    })?;
    let mut guard = StreamingChildGuard::new(child);
    let stdin = guard
        .child_mut()
        .stdin
        .take()
        .ok_or_else(|| StreamLeaderError {
            error: Error::Provider(format!("team stream {program}: missing stdin")),
            emitted: false,
        })?;
    let stdout = guard
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| StreamLeaderError {
            error: Error::Provider(format!("team stream {program}: missing stdout")),
            emitted: false,
        })?;
    let stderr = guard
        .child_mut()
        .stderr
        .take()
        .ok_or_else(|| StreamLeaderError {
            error: Error::Provider(format!("team stream {program}: missing stderr")),
            emitted: false,
        })?;
    let stderr_reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut bytes).await;
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let prompt_bytes = prompt.as_bytes().to_vec();
    let write_input = async move {
        let mut stdin = stdin;
        stdin
            .write_all(&prompt_bytes)
            .await
            .map_err(|e| Error::Provider(format!("team stream write stdin {program}: {e}")))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| Error::Provider(format!("team stream close stdin {program}: {e}")))
    };

    let mut lines = BufReader::new(stdout).lines();
    let mut output = String::new();
    let mut emitted = false;
    let read_output = async {
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| Error::Provider(format!("team stream read {program}: {e}")))?
        {
            match parse_stream_line(target, &line) {
                ParsedStreamLine::Text(text) if !text.is_empty() => {
                    output.push_str(&text);
                    tx.send(Ok(text))
                        .await
                        .map_err(|_| Error::Provider("team stream receiver closed".to_string()))?;
                    emitted = true;
                }
                ParsedStreamLine::Progress(progress) => {
                    tx.send(Ok(progress))
                        .await
                        .map_err(|_| Error::Provider("team stream receiver closed".to_string()))?;
                }
                ParsedStreamLine::Error(error) => return Err(Error::Provider(error)),
                _ => {}
            }
        }
        Ok::<(), Error>(())
    };
    let run = async {
        tokio::try_join!(write_input, read_output)?;
        let status = guard
            .child_mut()
            .wait()
            .await
            .map_err(|e| Error::Provider(format!("team stream wait {program}: {e}")))?;
        Ok(status)
    };
    let status = tokio::time::timeout(timeout, run).await;
    if matches!(&status, Err(_) | Ok(Err(_))) {
        guard.terminate();
    }
    let stderr = tokio::time::timeout(Duration::from_millis(250), stderr_reader)
        .await
        .ok()
        .and_then(|result| result.ok())
        .unwrap_or_default();
    match status {
        Err(_) => Err(StreamLeaderError {
            error: Error::Provider(format!(
                "team stream {program} timed out after {}s",
                timeout.as_secs()
            )),
            emitted,
        }),
        Ok(Err(error)) => Err(StreamLeaderError { error, emitted }),
        Ok(Ok(status)) if !status.success() => Err(StreamLeaderError {
            error: Error::Provider(format!(
                "team stream {program} exited with {}: {}",
                status
                    .code()
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                safe_cli_error(&stderr)
            )),
            emitted,
        }),
        Ok(Ok(_)) => {
            guard.disarm();
            usage_ledger::record_invocation(
                &leader_usage_target(target, model),
                model.unwrap_or_default(),
                usage_ledger::estimate_tokens(prompt),
                usage_ledger::estimate_tokens(&output),
                true,
                true,
            );
            Ok(())
        }
    }
}

enum ParsedStreamLine {
    Text(String),
    Progress(String),
    Error(String),
    Ignore,
}

fn parse_stream_line(target: &str, line: &str) -> ParsedStreamLine {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return if matches!(target, "claude" | "opencode") {
            ParsedStreamLine::Ignore
        } else {
            ParsedStreamLine::Text(format!("{line}\n"))
        };
    };
    match target {
        "claude" => {
            if value.get("type").and_then(|v| v.as_str()) == Some("stream_event")
                && value.pointer("/event/type").and_then(|v| v.as_str())
                    == Some("content_block_start")
                && value
                    .pointer("/event/content_block/type")
                    .and_then(|v| v.as_str())
                    == Some("tool_use")
            {
                return value
                    .pointer("/event/content_block/name")
                    .and_then(|v| v.as_str())
                    .map(tool_progress)
                    .unwrap_or(ParsedStreamLine::Ignore);
            }
            if value.get("type").and_then(|v| v.as_str()) == Some("stream_event")
                && value.pointer("/event/type").and_then(|v| v.as_str())
                    == Some("content_block_delta")
                && value.pointer("/event/delta/type").and_then(|v| v.as_str()) == Some("text_delta")
            {
                return value
                    .pointer("/event/delta/text")
                    .and_then(|v| v.as_str())
                    .map(|text| ParsedStreamLine::Text(text.to_string()))
                    .unwrap_or(ParsedStreamLine::Ignore);
            }
            if value.get("type").and_then(|v| v.as_str()) == Some("result")
                && value.get("is_error").and_then(|v| v.as_bool()) == Some(true)
            {
                let raw = value
                    .get("errors")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .filter(|message| !message.is_empty())
                    .unwrap_or_else(|| "Claude leader returned an error".to_string());
                return ParsedStreamLine::Error(safe_cli_error(&raw));
            }
            ParsedStreamLine::Ignore
        }
        "opencode" => {
            if value.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                return value
                    .pointer("/part/tool")
                    .and_then(|v| v.as_str())
                    .map(tool_progress)
                    .unwrap_or(ParsedStreamLine::Ignore);
            }
            if value.get("type").and_then(|v| v.as_str()) == Some("text") {
                return value
                    .pointer("/part/text")
                    .and_then(|v| v.as_str())
                    .map(|text| ParsedStreamLine::Text(text.to_string()))
                    .unwrap_or(ParsedStreamLine::Ignore);
            }
            ParsedStreamLine::Ignore
        }
        _ => ParsedStreamLine::Ignore,
    }
}

fn tool_progress(tool: &str) -> ParsedStreamLine {
    let label = if tool.contains("memory_recall") || tool.contains("memory_smart_search") {
        "recalling relevant memory"
    } else if tool.contains("memory_timeline") || tool.contains("memory_sessions") {
        "loading recent session context"
    } else if tool.contains("agent_call") || tool == "task" {
        "delegating worker"
    } else {
        "Leader tool running"
    };
    ParsedStreamLine::Progress(format!("[rtrt] {label}\n"))
}

fn leader_display_name(leader: &RankedTarget) -> String {
    match (leader.target.as_str(), leader.model.as_deref()) {
        ("claude", Some("opus")) => "Claude Opus".to_string(),
        ("claude", Some("sonnet")) => "Claude Sonnet".to_string(),
        ("opencode", Some(model)) if model.starts_with("openai/") => "GPT".to_string(),
        ("opencode", Some(model)) if model.starts_with("opencode-go/") => "GLM".to_string(),
        ("opencode", Some(model)) if model.contains("kimi") => "Kimi".to_string(),
        (target, Some(model)) => format!("{target}/{model}"),
        (target, None) => target.to_string(),
    }
}

fn safe_cli_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if ["quota", "rate limit", "rate-limit", "429", "hit your limit"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        "provider quota or rate limit reached".to_string()
    } else if ["401", "403", "unauthorized", "forbidden", "authentication"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        "provider authentication failed".to_string()
    } else if lower.contains("overloaded") || lower.contains("capacity") {
        "provider overloaded or at capacity".to_string()
    } else {
        "leader process failed; raw transport error suppressed".to_string()
    }
}

fn leader_usage_target(target: &str, model: Option<&str>) -> String {
    if target == "opencode"
        && let Some((provider, _)) = model.and_then(|model| model.split_once('/'))
    {
        return provider.to_string();
    }
    target.to_string()
}

struct StreamingChildGuard {
    child: Option<tokio::process::Child>,
    #[cfg(unix)]
    process_group: Option<Pid>,
}

impl StreamingChildGuard {
    fn new(child: tokio::process::Child) -> Self {
        #[cfg(unix)]
        let process_group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw);
        Self {
            child: Some(child),
            #[cfg(unix)]
            process_group,
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("streaming child guard armed")
    }

    fn disarm(&mut self) {
        self.child.take();
    }

    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        if let Some(process_group) = self.process_group {
            let _ = kill(Pid::from_raw(-process_group.as_raw()), Signal::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(pid) = child.id() {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = child.start_kill();
    }
}

impl Drop for StreamingChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
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
         Recover prior context through rtrt memory_recall, memory_smart_search, memory_timeline, or memory_sessions when the current user message depends on earlier work; the router intentionally forwards no chat history.\n\
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

fn ranked_leaders(config: &TeamConfig, usage: &UsageSnapshot) -> Result<Vec<RankedTarget>> {
    if !config.enabled {
        return Err(Error::Config(
            "team must be enabled before dispatch".to_string(),
        ));
    }
    config.validate()?;

    let mut leaders = config
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
        .collect::<Result<Vec<_>>>()?;
    // Preserve the configured quality order exactly, except that leaders whose
    // provider budget is known exhausted move behind every usable/unknown
    // leader. No prompt content participates in this decision.
    leaders.sort_by_key(|leader| leader_is_exhausted(leader, usage));
    Ok(leaders)
}

fn leader_is_exhausted(leader: &RankedTarget, usage: &UsageSnapshot) -> bool {
    let model_provider = leader
        .model
        .as_deref()
        .and_then(|model| model.split_once('/').map(|(provider, _)| provider));
    model_provider
        .and_then(|provider| usage.headroom(provider))
        .or_else(|| usage.headroom(&leader.target))
        .is_some_and(|headroom| {
            headroom.remaining == 0
                || headroom
                    .request_remaining
                    .is_some_and(|remaining| remaining == 0)
        })
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
        let leaders = ranked_leaders(&config(), &UsageSnapshot::default()).unwrap();

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

        let error = ranked_leaders(&config, &UsageSnapshot::default()).unwrap_err();
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

        let error = ranked_leaders(&config, &UsageSnapshot::default()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("references unknown member: missing")
        );
    }

    #[test]
    fn claude_stream_parser_emits_only_text_delta() {
        let text = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}},"session_id":"secret-ish-session-id","uuid":"trace-id"}"#;
        let tool_start = r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"mcp__rtrt__agent_call","input":{"prompt":"private"}}},"session_id":"session"}"#;
        let tool_json = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"path\":\"/private/file\"}"}},"session_id":"session"}"#;

        assert!(matches!(
            parse_stream_line("claude", text),
            ParsedStreamLine::Text(value) if value == "hello"
        ));
        assert!(matches!(
            parse_stream_line("claude", tool_start),
            ParsedStreamLine::Progress(value)
                if value == "[rtrt] delegating worker\n" && !value.contains("private")
        ));
        assert!(matches!(
            parse_stream_line("claude", tool_json),
            ParsedStreamLine::Ignore
        ));
    }

    #[test]
    fn opencode_stream_parser_ignores_tool_metadata() {
        let text = r#"{"type":"text","part":{"text":"done"},"sessionID":"session"}"#;
        let tool =
            r#"{"type":"tool_use","part":{"tool":"bash","state":{"input":{"command":"secret"}}}}"#;

        assert!(matches!(
            parse_stream_line("opencode", text),
            ParsedStreamLine::Text(value) if value == "done"
        ));
        assert!(matches!(
            parse_stream_line("opencode", tool),
            ParsedStreamLine::Progress(value)
                if value == "[rtrt] Leader tool running\n" && !value.contains("secret")
        ));
    }

    #[test]
    fn streamed_errors_keep_classification_without_leaking_raw_values() {
        let raw = "429 quota exceeded token=private-value /home/user/project";
        let safe = safe_cli_error(raw);

        assert_eq!(safe, "provider quota or rate limit reached");
        assert!(!safe.contains("private-value"));
        assert!(!safe.contains("/home/user"));
    }

    #[test]
    fn opencode_leader_usage_is_attributed_to_actual_provider() {
        assert_eq!(
            leader_usage_target("opencode", Some("openai/gpt-5.6-sol")),
            "openai"
        );
        assert_eq!(
            leader_usage_target("opencode", Some("opencode-go/glm-5.2")),
            "opencode-go"
        );
        assert_eq!(leader_usage_target("claude", Some("opus")), "claude");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_timeout_kills_child_before_bounded_pipe_drain() {
        let (tx, _rx) = mpsc::channel(1);
        let started = std::time::Instant::now();
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 30 & wait".to_string(),
        ];

        let result = run_streaming_cli(
            "test",
            None,
            "prompt",
            &argv,
            Duration::from_millis(100),
            &TeamExecutionContext::default(),
            &tx,
        )
        .await;

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_cli_forwards_opencode_parent_context() {
        let (tx, mut rx) = mpsc::channel(16);
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat >/dev/null; printf '%s|%s' \"$RTRT_OPENCODE_PARENT_SESSION\" \"$RTRT_OPENCODE_SERVER_URL\""
                .to_string(),
        ];
        let context = TeamExecutionContext {
            opencode_session_id: Some("ses_native_child".to_string()),
            opencode_server_url: Some("http://127.0.0.1:4096/".to_string()),
        };

        let output = run_streaming_cli(
            "test",
            None,
            "prompt",
            &argv,
            Duration::from_secs(2),
            &context,
            &tx,
        )
        .await
        .unwrap();
        drop(tx);
        while rx.recv().await.is_some() {}

        assert_eq!(output, "ses_native_child|http://127.0.0.1:4096/");
    }

    #[test]
    fn streaming_args_use_text_event_modes_and_leader_agent() {
        let prompt = "prompt too large for argv";
        let mut claude = vec!["claude".to_string(), "-p".to_string()];
        claude.push(prompt.to_string());
        remove_prompt_argument(&mut claude, prompt);
        enable_json_stream("claude", &mut claude);
        assert!(!claude.iter().any(|arg| arg == prompt));
        assert!(
            claude
                .windows(2)
                .any(|args| args == ["--output-format", "stream-json"])
        );
        assert!(claude.iter().any(|arg| arg == "--include-partial-messages"));

        let mut opencode = vec![
            "opencode".to_string(),
            "run".to_string(),
            "--agent".to_string(),
            "build".to_string(),
        ];
        enable_json_stream("opencode", &mut opencode);
        assert_eq!(opencode[3], "rtrt-leader");
        assert!(opencode.windows(2).any(|args| args == ["--format", "json"]));
    }

    #[test]
    fn multi_megabyte_prompt_is_removed_before_spawn() {
        let prompt = "x".repeat(3 * 1024 * 1024);
        let mut argv =
            template_to_argv("claude -p {model_args} {prompt}", &prompt, Some("opus")).unwrap();

        remove_prompt_argument(&mut argv, &prompt);

        assert!(!argv.iter().any(|argument| argument == &prompt));
        assert!(argv.iter().map(String::len).sum::<usize>() < 1024);
    }

    #[test]
    fn exhausted_provider_moves_to_end_without_content_routing() {
        let usage = UsageSnapshot::from_usage_and_limits_for_tests(
            [("openai", 100), ("claude", 10)],
            [("openai", 100), ("claude", 100)],
        );

        let leaders = ranked_leaders(&config(), &usage).unwrap();

        assert_eq!(leaders[0].target, "claude");
        assert_eq!(leaders[1].target, "opencode");
    }
}
