use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use rtrt_core::{CostClass, DetectedTool, Error, InvocationMode, Result};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::{ChatMessage, ChatRequest, Gateway, Role, router::RankedTarget, usage_ledger};

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

const CHILD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROMPT_PLACEHOLDER: &str = "{prompt}";
const MODEL_PLACEHOLDER: &str = "{model}";
const ASCII_SPINNER_CHARS: &[char] = &['|', '/', '-', '\\'];
const BRAILLE_SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone)]
pub struct InvokeOptions {
    pub mode: Option<Mode>,
    pub model: Option<String>,
    pub timeout: Duration,
}

impl Default for InvokeOptions {
    fn default() -> Self {
        Self {
            mode: Some(Mode::Auto),
            model: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Cli,
    Api,
    Auto,
}

impl Mode {
    pub fn parse_label(value: &str) -> Result<Self> {
        match value {
            "cli" => Ok(Self::Cli),
            "api" => Ok(Self::Api),
            "auto" => Ok(Self::Auto),
            other => Err(Error::Provider(format!(
                "invoke: unknown mode '{other}' (expected cli, api, or auto)"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeOutcome {
    pub target: String,
    pub mode_used: Mode,
    pub model: Option<String>,
    pub output: String,
    pub exit_code: Option<i32>,
    pub ms: u64,
}

pub async fn invoke_agent(
    target: &str,
    prompt: &str,
    opts: InvokeOptions,
) -> Result<InvokeOutcome> {
    let tools = rtrt_core::detect_tools();
    let tool = resolve_target(target, &tools)?;
    let requested = opts.mode.unwrap_or(Mode::Auto);
    let mode_used = select_mode(tool, requested)?;
    let model = opts.model.clone().or_else(|| tool.models.first().cloned());
    let started = Instant::now();

    // Per-mode invocation. On any failure we still record the request (with
    // `ok = 0`) before propagating the error, so the ledger reflects spent
    // request budget even for failed calls.
    let ledger_model = model.clone().unwrap_or_default();
    let (output, exit_code) = match mode_used {
        Mode::Cli => {
            let template = match tool.cli_invocation.as_deref() {
                Some(template) => template,
                None => {
                    record_cli(&tool.name, &ledger_model, prompt, "", false);
                    return Err(Error::Provider(format!(
                        "invoke: target '{}' has no CLI invocation",
                        tool.name
                    )));
                }
            };
            let argv = match template_to_argv(template, prompt, model.as_deref()) {
                Ok(argv) => argv,
                Err(err) => {
                    record_cli(&tool.name, &ledger_model, prompt, "", false);
                    return Err(err);
                }
            };
            match run_cli_argv(&argv, opts.timeout).await {
                Ok((output, exit_code)) => {
                    // CLI shell-outs report no usage; estimate from chars/4 and
                    // mark the row as estimated. `ok` follows the exit code.
                    let ok = exit_code.unwrap_or(0) == 0;
                    record_cli(&tool.name, &ledger_model, prompt, &output, ok);
                    (output, exit_code)
                }
                Err(err) => {
                    record_cli(&tool.name, &ledger_model, prompt, "", false);
                    return Err(err);
                }
            }
        }
        Mode::Api => {
            let model = match model.as_deref() {
                Some(model) => model,
                None => {
                    record_cli(&tool.name, &ledger_model, prompt, "", false);
                    return Err(Error::Provider(format!(
                        "invoke: target '{}' API mode requires --model",
                        tool.name
                    )));
                }
            };
            let req = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: prompt.to_string(),
                }],
                max_tokens: Some(api_max_tokens()),
                temperature: None,
            };
            // This path keeps its own ledger rows (attributed to the detected
            // tool name, including pre-dispatch failures), so the gateway's
            // own recording is switched off to avoid double-counting.
            match Gateway::from_env()
                .with_usage_recording(false)
                .chat(req)
                .await
            {
                Ok(resp) => {
                    // API mode returns real token counts; record them exactly.
                    usage_ledger::record_invocation(
                        &tool.name,
                        model,
                        resp.usage.input_tokens,
                        resp.usage.output_tokens,
                        false,
                        true,
                    );
                    (resp.content, None)
                }
                Err(err) => {
                    record_cli(&tool.name, model, prompt, "", false);
                    return Err(err);
                }
            }
        }
        Mode::Auto => {
            return Err(Error::Provider(
                "invoke: internal error: auto mode was not resolved".to_string(),
            ));
        }
    };

    Ok(InvokeOutcome {
        target: tool.name.clone(),
        mode_used,
        model,
        output,
        exit_code,
        ms: started.elapsed().as_millis() as u64,
    })
}

/// One failed candidate in a failover walk, kept for the aggregated error and
/// the result's audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverAttempt {
    pub target: String,
    pub retryable: bool,
    pub error: String,
}

/// The outcome of an [`invoke_with_failover`] walk: the successful invocation
/// plus how many candidates were tried before it served the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverOutcome {
    /// The invocation that succeeded.
    pub outcome: InvokeOutcome,
    /// Targets that failed (in order) before this one served the request.
    pub failed_over: Vec<FailoverAttempt>,
}

impl FailoverOutcome {
    /// How many candidates fell over (retryable failures) before success.
    pub fn fell_over(&self) -> usize {
        self.failed_over.len()
    }

    /// A one-line audit string, e.g. `served by openai after 2 fell over
    /// (ollama: retryable, claude: retryable)`.
    pub fn summary(&self) -> String {
        if self.failed_over.is_empty() {
            return format!("served by {} (no failover)", self.outcome.target);
        }
        let trail = self
            .failed_over
            .iter()
            .map(|a| format!("{}: {}", a.target, classify_label(a.retryable)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "served by {} after {} fell over ({trail})",
            self.outcome.target,
            self.failed_over.len()
        )
    }
}

/// Classify an invocation [`Error`] as retryable (fall over to the next ranked
/// candidate) versus terminal (return immediately).
///
/// We are deliberately conservative: only failures that another provider could
/// plausibly satisfy are retryable. Rate-limit / quota / 429 / 5xx / timeouts /
/// process-spawn failures fall over. Auth, other 4xx, empty-prompt, and
/// unknown-target are user/config errors — falling over would just repeat the
/// same mistake — so they are terminal.
pub fn is_retryable_error(err: &Error) -> bool {
    // Errors surface as `Error::Provider(String)`; we classify on the message.
    let Error::Provider(message) = err else {
        // I/O and serde errors are transient/local plumbing, not provider state
        // a peer would share — treat as retryable so failover can try a peer.
        return true;
    };
    let lower = message.to_ascii_lowercase();

    // Terminal first: these must NOT fall over even if a status-code substring
    // would otherwise look retryable.
    const TERMINAL_MARKERS: &[&str] = &[
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "authentication",
        "api key",
        "not detected",
        "not installed",
        "disabled",
        "does not support",
        "requires --model",
        "needs --model",
        "no cli invocation",
        "is empty",
        "available targets",
    ];
    if TERMINAL_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }

    // Retryable: rate limits, quota, server-side and transient failures, and
    // local spawn/timeout errors a different target could route around.
    const RETRYABLE_MARKERS: &[&str] = &[
        "429",
        "rate limit",
        "rate-limit",
        "ratelimit",
        "quota",
        "overloaded",
        "capacity",
        "too many requests",
        "timed out",
        "timeout",
        "spawn",
        "500",
        "502",
        "503",
        "504",
        "budget exceeded",
    ];
    RETRYABLE_MARKERS.iter().any(|m| lower.contains(m))
}

fn classify_label(retryable: bool) -> &'static str {
    if retryable { "retryable" } else { "terminal" }
}

/// Invoke targets in ranked order with automatic cross-provider failover.
///
/// Tries each [`RankedTarget`] at most once. On a **retryable** failure
/// (rate-limit / quota / 429 / 5xx / timeout / spawn) it records the attempt and
/// falls through to the next candidate. On a **terminal** failure (auth, other
/// 4xx, empty prompt, unknown/disabled target) it stops immediately and returns
/// that error — falling over would only repeat a user/config mistake. Returns
/// the first success (with its failover trail), or an aggregated error listing
/// every attempt if all candidates fail.
///
/// `invoke_agent`'s single-target behavior is untouched; this is an additional
/// layer on top. Each underlying call still records to the ledger (failures as
/// `ok = 0`), so balance accounting is identical to a direct invocation.
pub async fn invoke_with_failover(
    targets: &[RankedTarget],
    prompt: &str,
    timeout: Duration,
) -> Result<FailoverOutcome> {
    if targets.is_empty() {
        return Err(Error::Provider(
            "invoke: failover received no ranked targets".to_string(),
        ));
    }

    let mut failed_over = Vec::new();
    for candidate in targets {
        let opts = InvokeOptions {
            mode: Some(candidate.mode),
            model: candidate.model.clone(),
            timeout,
        };
        match invoke_agent(&candidate.target, prompt, opts).await {
            Ok(outcome) => {
                return Ok(FailoverOutcome {
                    outcome,
                    failed_over,
                });
            }
            Err(err) => {
                let retryable = is_retryable_error(&err);
                failed_over.push(FailoverAttempt {
                    target: candidate.target.clone(),
                    retryable,
                    error: err.to_string(),
                });
                // Terminal failure: do not fall over — repeating a user/config
                // mistake against the next target would not help.
                if !retryable {
                    return Err(aggregated_error(&failed_over));
                }
                // Retryable: the ledger already recorded ok=0 inside
                // invoke_agent; walk on to the next ranked candidate.
            }
        }
    }
    Err(aggregated_error(&failed_over))
}

/// Build a single error summarizing every failover attempt, in order.
fn aggregated_error(attempts: &[FailoverAttempt]) -> Error {
    let trail = attempts
        .iter()
        .map(|a| {
            format!(
                "{} ({}): {}",
                a.target,
                classify_label(a.retryable),
                a.error
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Error::Provider(format!(
        "invoke: all {} candidate(s) failed: {trail}",
        attempts.len()
    ))
}

/// Output-token ceiling for API-mode invocations: `RTRT_API_MAX_TOKENS` env
/// var → the effective (global ⊕ project) `[providers] api_max_tokens` →
/// [`rtrt_core::DEFAULT_API_MAX_TOKENS`]. Previously a hardcoded 1024, which
/// silently truncated routed API answers.
fn api_max_tokens() -> u32 {
    rtrt_core::Config::load_effective_for_cwd()
        .providers
        .effective_api_max_tokens()
}

/// Record an estimated-token ledger row from a prompt/output text pair
/// (`chars / 4`). Used for CLI shell-outs and for any failed invocation where
/// we have no real usage to report.
fn record_cli(target: &str, model: &str, prompt: &str, output: &str, ok: bool) {
    usage_ledger::record_invocation(
        target,
        model,
        usage_ledger::estimate_tokens(prompt),
        usage_ledger::estimate_tokens(output),
        true,
        ok,
    );
}

pub fn template_to_argv(template: &str, prompt: &str, model: Option<&str>) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    for part in template.split_whitespace() {
        match part {
            PROMPT_PLACEHOLDER => argv.push(prompt.to_string()),
            MODEL_PLACEHOLDER => {
                let model = model.ok_or_else(|| {
                    Error::Provider("invoke: CLI template requires --model".to_string())
                })?;
                argv.push(model.to_string());
            }
            literal => argv.push(literal.to_string()),
        }
    }
    if argv.is_empty() {
        return Err(Error::Provider(
            "invoke: CLI invocation template is empty".to_string(),
        ));
    }
    Ok(argv)
}

fn resolve_target<'a>(target: &str, tools: &'a [DetectedTool]) -> Result<&'a DetectedTool> {
    let normalized = target.to_ascii_lowercase();
    let found = tools
        .iter()
        .find(|tool| tool.name == target || tool.name == normalized);
    let Some(tool) = found else {
        return Err(target_unavailable_error(target, tools, "not detected"));
    };
    if !tool.installed {
        return Err(target_unavailable_error(target, tools, "not installed"));
    }
    if !tool.enabled {
        return Err(target_unavailable_error(target, tools, "disabled"));
    }
    Ok(tool)
}

fn target_unavailable_error(target: &str, tools: &[DetectedTool], reason: &str) -> Error {
    let available = available_targets(tools);
    Error::Provider(format!(
        "invoke: target '{target}' is {reason}; available targets: {available}"
    ))
}

fn available_targets(tools: &[DetectedTool]) -> String {
    let mut names = tools
        .iter()
        .filter(|tool| tool.installed && tool.enabled)
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        "(none)".to_string()
    } else {
        names.join(", ")
    }
}

fn select_mode(tool: &DetectedTool, requested: Mode) -> Result<Mode> {
    match requested {
        Mode::Cli => {
            if tool.invocation_modes.contains(&InvocationMode::Cli) && tool.cli_invocation.is_some()
            {
                Ok(Mode::Cli)
            } else {
                Err(Error::Provider(format!(
                    "invoke: target '{}' does not support CLI mode",
                    tool.name
                )))
            }
        }
        Mode::Api => {
            if tool.invocation_modes.contains(&InvocationMode::Api) {
                Ok(Mode::Api)
            } else {
                Err(Error::Provider(format!(
                    "invoke: target '{}' does not support API mode",
                    tool.name
                )))
            }
        }
        Mode::Auto => Ok(auto_mode_for(tool)),
    }
}

fn auto_mode_for(tool: &DetectedTool) -> Mode {
    let cheap_cli = matches!(
        tool.cost_class,
        CostClass::LocalFree | CostClass::SubscriptionFlat
    );
    if cheap_cli
        && tool.invocation_modes.contains(&InvocationMode::Cli)
        && tool.cli_invocation.is_some()
    {
        Mode::Cli
    } else {
        Mode::Api
    }
}

async fn run_cli_argv(argv: &[String], timeout: Duration) -> Result<(String, Option<i32>)> {
    let (program, args) = argv.split_first().ok_or_else(|| {
        Error::Provider("invoke: cannot spawn an empty CLI invocation".to_string())
    })?;
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Provider(format!("invoke: spawn '{program}': {e}")))?;

    let stdout_reader = child.stdout.take().map(read_pipe);
    let stderr_reader = child.stderr.take().map(read_pipe);

    let status = match tokio::time::timeout(timeout, wait_for_child(&mut child)).await {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_reader(stdout_reader).await;
            let _ = join_reader(stderr_reader).await;
            return Err(Error::Provider(format!(
                "invoke: command '{}' timed out after {}s",
                program,
                timeout.as_secs()
            )));
        }
    };

    let stdout = join_reader(stdout_reader).await?;
    let stderr = join_reader(stderr_reader).await?;
    let mut output = String::new();
    output.push_str(&String::from_utf8_lossy(&stdout));
    output.push_str(&String::from_utf8_lossy(&stderr));
    let output = sanitize_cli_output(&output);
    Ok((output, status.code()))
}

fn sanitize_cli_output(input: &str) -> String {
    let without_ansi = strip_ansi_escape_sequences(input);
    let mut output = String::new();
    let mut frame = String::new();
    let mut previous_was_cr = false;
    for ch in without_ansi.chars() {
        match ch {
            '\r' => {
                push_non_spinner_frame(&mut output, &frame, true);
                frame.clear();
                previous_was_cr = true;
            }
            '\n' => {
                if previous_was_cr {
                    previous_was_cr = false;
                    continue;
                }
                push_non_spinner_frame(&mut output, &frame, false);
                output.push('\n');
                frame.clear();
            }
            _ => {
                previous_was_cr = false;
                frame.push(ch);
            }
        }
    }
    push_non_spinner_frame(&mut output, &frame, false);
    output.trim().to_string()
}

fn strip_ansi_escape_sequences(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            output.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                let _ = chars.next();
                let mut saw_escape = false;
                for next in chars.by_ref() {
                    if next == '\u{7}' {
                        break;
                    }
                    if saw_escape && next == '\\' {
                        break;
                    }
                    saw_escape = next == '\x1b';
                }
            }
            Some('\u{40}'..='\u{5f}') => {
                let _ = chars.next();
            }
            Some(_) | None => {}
        }
    }
    output
}

fn push_non_spinner_frame(output: &mut String, frame: &str, add_line_break: bool) {
    if is_spinner_only_frame(frame) {
        return;
    }
    output.push_str(frame);
    if add_line_break {
        output.push('\n');
    }
}

fn is_spinner_only_frame(frame: &str) -> bool {
    let trimmed = frame.trim();
    !trimmed.is_empty()
        && trimmed.chars().all(|ch| {
            ch.is_whitespace()
                || ASCII_SPINNER_CHARS.contains(&ch)
                || BRAILLE_SPINNER_CHARS.contains(&ch)
        })
}

fn read_pipe<R>(mut pipe: R) -> JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        pipe.read_to_end(&mut buf)?;
        Ok(buf)
    })
}

async fn join_reader(reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>) -> Result<Vec<u8>> {
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
    let bytes = reader
        .await
        .map_err(|e| Error::Provider(format!("invoke: output reader task failed: {e}")))??;
    Ok(bytes)
}

async fn wait_for_child(child: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => tokio::time::sleep(CHILD_WAIT_POLL_INTERVAL).await,
            Err(e) => return Err(Error::Provider(format!("invoke: wait failed: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use rtrt_core::{Capability, ToolKind};

    use super::*;

    #[test]
    fn template_substitution_keeps_prompt_and_model_as_single_args() {
        let argv = template_to_argv(
            "ollama run {model} {prompt}",
            "say hi in 3 words",
            Some("gemma3:4b-it-qat"),
        )
        .expect("template should parse");

        assert_eq!(
            argv,
            vec!["ollama", "run", "gemma3:4b-it-qat", "say hi in 3 words"]
        );
    }

    #[test]
    fn auto_mode_prefers_flat_or_free_cli_and_uses_api_otherwise() {
        let cli_tool = tool_for_mode(
            vec![InvocationMode::Cli, InvocationMode::Api],
            Some("claude -p {prompt}"),
            CostClass::SubscriptionFlat,
        );
        assert_eq!(auto_mode_for(&cli_tool), Mode::Cli);

        let api_tool = tool_for_mode(
            vec![InvocationMode::Cli, InvocationMode::Api],
            Some("gemini {prompt}"),
            CostClass::ApiMetered,
        );
        assert_eq!(auto_mode_for(&api_tool), Mode::Api);
    }

    #[test]
    fn classifies_rate_limit_and_5xx_and_timeout_as_retryable() {
        for message in [
            "anthropic 429: rate limit exceeded",
            "openai 503 Service Unavailable",
            "gateway: budget exceeded for openai",
            "invoke: command 'ollama' timed out after 120s",
            "invoke: spawn 'codex': No such file or directory",
            "provider overloaded, retry later",
            "daily quota reached",
        ] {
            let err = Error::Provider(message.to_string());
            assert!(
                is_retryable_error(&err),
                "expected retryable for: {message}"
            );
        }
    }

    #[test]
    fn classifies_auth_and_config_errors_as_terminal() {
        for message in [
            "anthropic 401: invalid api key",
            "openai 403: forbidden",
            "invoke: target 'foo' is not detected; available targets: claude",
            "invoke: target 'claude' does not support API mode",
            "invoke: target 'openai' API mode requires --model",
            "rtrt call: prompt is empty",
        ] {
            let err = Error::Provider(message.to_string());
            assert!(
                !is_retryable_error(&err),
                "expected terminal for: {message}"
            );
        }
    }

    #[test]
    fn terminal_markers_win_over_status_substrings() {
        // A 401 that also mentions "rate limit" must stay terminal: an auth
        // failure will not be fixed by falling over to another provider.
        let err = Error::Provider("anthropic 401: rate limit note".to_string());
        assert!(!is_retryable_error(&err));
    }

    #[tokio::test]
    async fn failover_stops_immediately_on_terminal_first_candidate() {
        // An unknown target yields a terminal "not detected" error; failover
        // must return immediately without walking the (healthy) second target.
        let targets = vec![
            ranked("__definitely_not_a_real_target__"),
            ranked("__second_unreachable_target__"),
        ];
        let err = invoke_with_failover(&targets, "hi", Duration::from_secs(1))
            .await
            .expect_err("terminal first candidate should fail");
        let msg = err.to_string();
        // Exactly one attempt recorded — the second target was never tried.
        assert!(msg.contains("all 1 candidate(s) failed"), "got: {msg}");
        assert!(msg.contains("__definitely_not_a_real_target__"));
        assert!(!msg.contains("__second_unreachable_target__"));
    }

    #[tokio::test]
    async fn failover_rejects_empty_target_list() {
        let err = invoke_with_failover(&[], "hi", Duration::from_secs(1))
            .await
            .expect_err("empty list should error");
        assert!(err.to_string().contains("no ranked targets"));
    }

    #[test]
    fn failover_summary_reports_served_target_and_count() {
        let outcome = FailoverOutcome {
            outcome: InvokeOutcome {
                target: "openai".to_string(),
                mode_used: Mode::Api,
                model: Some("gpt-x".to_string()),
                output: "ok".to_string(),
                exit_code: None,
                ms: 1,
            },
            failed_over: vec![FailoverAttempt {
                target: "ollama".to_string(),
                retryable: true,
                error: "ollama 429".to_string(),
            }],
        };
        assert_eq!(outcome.fell_over(), 1);
        let summary = outcome.summary();
        assert!(summary.contains("served by openai after 1 fell over"));
        assert!(summary.contains("ollama: retryable"));
    }

    fn ranked(name: &str) -> RankedTarget {
        RankedTarget {
            target: name.to_string(),
            mode: Mode::Auto,
            model: None,
            cost_class: CostClass::Unknown,
        }
    }

    #[test]
    fn sanitize_cli_output_removes_spinner_frames_and_ansi() {
        let raw = "\x1b[?25l\r\x1b[?2026h⠙\r\x1b[K⠹\r\x1b[32mClean answer\x1b[0m\n";

        assert_eq!(sanitize_cli_output(raw), "Clean answer");
    }

    fn tool_for_mode(
        invocation_modes: Vec<InvocationMode>,
        cli_invocation: Option<&str>,
        cost_class: CostClass,
    ) -> DetectedTool {
        DetectedTool {
            name: "test".to_string(),
            kind: ToolKind::CodingAgent,
            installed: true,
            path: None,
            version: None,
            invocation_modes,
            cli_invocation: cli_invocation.map(str::to_string),
            cost_class,
            capabilities: vec![Capability::Code],
            config_path: None,
            models: Vec::new(),
            server_running: None,
            enabled: true,
        }
    }
}
