use std::{
    future::Future,
    io::Read,
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use rtrt_core::{CostClass, DetectedTool, Error, InvocationMode, Result, config::FailoverConfig};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::{ChatMessage, ChatRequest, Gateway, Role, router::RankedTarget, usage_ledger};

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

const CHILD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const PROMPT_PLACEHOLDER: &str = "{prompt}";
const MODEL_PLACEHOLDER: &str = "{model}";
const MODEL_ARGS_PLACEHOLDER: &str = "{model_args}";
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
                Ok((output, Some(0))) => {
                    // CLI shell-outs report no usage; estimate from chars/4 and
                    // mark the row as estimated.
                    record_cli(&tool.name, &ledger_model, prompt, &output, true);
                    (output, Some(0))
                }
                Ok((output, exit_code)) => {
                    record_cli(&tool.name, &ledger_model, prompt, &output, false);
                    return Err(cli_exit_error(&argv[0], exit_code, &output));
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
        let served_by = target_label(&self.outcome.target, self.outcome.model.as_deref());
        if self.failed_over.is_empty() {
            return format!("served by {served_by} (no failover)");
        }
        let trail = self
            .failed_over
            .iter()
            .map(|a| {
                format!(
                    "{}: {}",
                    target_label(&a.target, a.model.as_deref()),
                    classify_label(a.retryable)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "served by {served_by} after {} fell over ({trail})",
            self.failed_over.len()
        )
    }
}

/// One failed candidate in a policy walk — the richer record that
/// [`FailoverAttempt`] flattens from. It carries the [`FailureClass`] and
/// whether the target's transient retry was consumed, so an operator can see
/// *why* the walk moved on instead of only *that* it did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyAttempt {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How the failure was classified.
    pub class: FailureClass,
    /// True when the target was retried in place before the walk gave up on it.
    #[serde(default)]
    pub retried: bool,
    pub error: String,
}

impl PolicyAttempt {
    fn label(&self) -> String {
        target_label(&self.target, self.model.as_deref())
    }

    fn trail_entry(&self) -> String {
        let retried = if self.retried { ", retried" } else { "" };
        format!("{}: {}{retried}", self.label(), self.class.label())
    }
}

impl From<&PolicyAttempt> for FailoverAttempt {
    fn from(attempt: &PolicyAttempt) -> Self {
        Self {
            target: attempt.target.clone(),
            model: attempt.model.clone(),
            retryable: attempt.class.is_recoverable(),
            error: attempt.error.clone(),
        }
    }
}

/// The full record of an [`invoke_with_policy`] walk: what served the request
/// (if anything), every candidate that failed, and the lane that halted the
/// walk on a [`FailureClass::Fatal`].
///
/// This is a separate type rather than a `halted` field bolted onto
/// [`FailoverOutcome`], because `FailoverOutcome` models a *successful* walk —
/// it owns an `InvokeOutcome` — while a fatal halt has no success to report.
/// [`Self::into_failover`] flattens back to that legacy shape, which is how
/// [`invoke_with_failover`] keeps its exact contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyOutcome {
    /// The invocation that served the request, if any candidate succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served: Option<InvokeOutcome>,
    /// Every candidate that failed, in walk order (the halted one included).
    #[serde(default)]
    pub attempts: Vec<PolicyAttempt>,
    /// The lane whose fatal failure halted the walk; no candidate after it was
    /// tried. `None` means the walk either succeeded or exhausted the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halted: Option<PolicyAttempt>,
}

impl PolicyOutcome {
    /// How many candidates fell over — i.e. failed and handed the request to
    /// the next lane. The halted candidate did not fall over, so it is excluded.
    pub fn fell_over(&self) -> usize {
        self.attempts.len() - usize::from(self.halted.is_some())
    }

    /// A one-line audit string covering all three endings: served, halted at a
    /// fatal lane, or every candidate exhausted.
    pub fn summary(&self) -> String {
        let trail = self
            .attempts
            .iter()
            .map(PolicyAttempt::trail_entry)
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(halted) = &self.halted {
            return format!(
                "halted at {} after {} fell over: fatal, no failover ({})",
                halted.label(),
                self.fell_over(),
                halted.error
            );
        }
        match &self.served {
            Some(outcome) => {
                let served_by = target_label(&outcome.target, outcome.model.as_deref());
                if self.attempts.is_empty() {
                    format!("served by {served_by} (no failover)")
                } else {
                    format!(
                        "served by {served_by} after {} fell over ({trail})",
                        self.attempts.len()
                    )
                }
            }
            None => format!("all {} candidate(s) failed ({trail})", self.attempts.len()),
        }
    }

    /// Flatten to the legacy shape: `Ok` with the failover trail when a
    /// candidate served the request, otherwise the aggregated error listing
    /// every attempt — byte-for-byte what `invoke_with_failover` returned
    /// before the three-class split.
    pub fn into_failover(self) -> Result<FailoverOutcome> {
        let failed_over: Vec<FailoverAttempt> =
            self.attempts.iter().map(FailoverAttempt::from).collect();
        match self.served {
            Some(outcome) => Ok(FailoverOutcome {
                outcome,
                failed_over,
            }),
            None => Err(aggregated_error(&failed_over)),
        }
    }
}

/// How the failover walk must react to a failed invocation.
///
/// This replaces the old retryable/terminal bool: the walk needs to know *why*
/// a lane failed, not merely whether some other lane could serve the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureClass {
    /// Timeout, 5xx, connection or spawn hiccup: the same target may well work
    /// a moment later, so it earns a short backoff and one more try before the
    /// walk spends another lane's budget.
    Transient,
    /// 429 / rate limit / quota / credits / usage cap: the target is out of
    /// allowance, so retrying it is pure latency — fall over immediately.
    Quota,
    /// Auth, unknown model, disabled or undetected target, malformed request:
    /// neither a retry nor a different provider can fix it. Halt and surface it
    /// to the operator.
    Fatal,
}

impl FailureClass {
    /// Lower-case label used in audit trails and aggregated errors.
    pub fn label(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Quota => "quota",
            Self::Fatal => "fatal",
        }
    }

    /// Whether the walk may continue past this failure (by retrying the same
    /// target or falling over). This is exactly the bool the pre-split
    /// `is_retryable_error` returned.
    pub fn is_recoverable(self) -> bool {
        !matches!(self, Self::Fatal)
    }
}

/// Fatal markers, checked before any other built-in table: an auth or config
/// mistake must not fall over even when the same message carries a
/// retryable-looking status code.
const FATAL_MARKERS: &[&str] = &[
    "401",
    "403",
    "unauthorized",
    "forbidden",
    "invalid api key",
    "authentication",
    "api key",
    "not detected",
    "model not found",
    "model-not-found",
    "disabled",
    "does not support",
    "requires --model",
    "needs --model",
    "no cli invocation",
    "is empty",
];

/// Quota markers: the target is out of budget or allowance. A sibling lane can
/// serve the request, but the same target cannot — so no same-target retry.
const QUOTA_MARKERS: &[&str] = &[
    "429",
    "rate limit",
    "rate-limit",
    "ratelimit",
    "quota",
    "weekly",
    "usage",
    "hit your",
    "limit reached",
    "credits exhausted",
    "budget exceeded",
    "too many requests",
];

/// Transient markers: a hiccup the same target may recover from on its own.
/// The 5xx heuristic ([`contains_server_status`]) joins this class.
const TRANSIENT_MARKERS: &[&str] = &[
    "overloaded",
    "capacity",
    "timed out",
    "timeout",
    "spawn",
    "not installed",
];

/// Same-target retries a transient failure earns before the walk falls over.
const DEFAULT_TRANSIENT_RETRIES: u32 = 1;

/// The transient backoff is the caller's own timeout budget divided by this,
/// so the wait always scales with how long the caller was willing to wait
/// (120s budget → 2s backoff, 2s budget → the 25ms floor) instead of being a
/// fixed constant that is either rude to fast calls or useless to slow ones.
const DEFAULT_BACKOFF_DIVISOR: u32 = 60;

/// The effective failure policy: the built-in marker tables plus any
/// `[failover]` overrides from the user's config.
///
/// Classification precedence, highest first:
///   1. user `fatal`, then user `quota`, then user `transient` — a user entry
///      therefore *reclassifies* a built-in marker;
///   2. built-in fatal, then built-in quota, then built-in transient
///      (including the 5xx heuristic);
///   3. anything still unmatched stays fatal, so an unknown provider message is
///      never silently retried around.
#[derive(Debug, Clone, Default)]
pub struct FailurePolicy {
    fatal: Vec<String>,
    quota: Vec<String>,
    transient: Vec<String>,
    transient_retries: Option<u32>,
    backoff_divisor: Option<u32>,
    backoff: Option<Duration>,
}

impl FailurePolicy {
    /// The shipped policy: built-in tables only, no user overrides.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Layer a user's `[failover]` section on top of the built-in tables. An
    /// all-default section yields exactly [`FailurePolicy::builtin`].
    pub fn from_config(config: &FailoverConfig) -> Self {
        Self {
            fatal: lowercased_markers(&config.fatal),
            quota: lowercased_markers(&config.quota),
            transient: lowercased_markers(&config.transient),
            transient_retries: config.transient_retries,
            backoff_divisor: config.backoff_divisor,
            backoff: config.backoff_ms.map(Duration::from_millis),
        }
    }

    /// Classify an invocation [`Error`] (see the type docs for precedence).
    pub fn classify(&self, err: &Error) -> FailureClass {
        // Errors surface as `Error::Provider(String)`; we classify on the
        // message. I/O and serde errors are local plumbing, not provider state
        // a peer would share, so they stay transient.
        let Error::Provider(message) = err else {
            return FailureClass::Transient;
        };
        let lower = message.to_ascii_lowercase();

        // 1. User overrides win outright, so an operator can move a marker
        //    between classes without a rebuild.
        for (markers, class) in [
            (&self.fatal, FailureClass::Fatal),
            (&self.quota, FailureClass::Quota),
            (&self.transient, FailureClass::Transient),
        ] {
            if markers.iter().any(|marker| lower.contains(marker.as_str())) {
                return class;
            }
        }

        // 2. Built-ins, fatal first.
        if FATAL_MARKERS.iter().any(|m| lower.contains(m)) {
            return FailureClass::Fatal;
        }
        if QUOTA_MARKERS.iter().any(|m| lower.contains(m)) {
            return FailureClass::Quota;
        }
        if TRANSIENT_MARKERS.iter().any(|m| lower.contains(m)) || contains_server_status(&lower) {
            return FailureClass::Transient;
        }

        // 3. Unmatched: conservative by design.
        FailureClass::Fatal
    }

    /// Same-target retries granted to a transient failure (`0` disables them).
    pub fn transient_retries(&self) -> u32 {
        self.transient_retries.unwrap_or(DEFAULT_TRANSIENT_RETRIES)
    }

    /// The pause before a same-target retry. Derived from the call's own
    /// timeout budget divided by [`DEFAULT_BACKOFF_DIVISOR`] (or the
    /// configured divisor), floored at one child-wait poll tick so it is never
    /// a no-op and ceilinged at the default budget's share so an unusually long
    /// timeout cannot park the walk. A configured `backoff_ms` pins it outright.
    pub fn backoff(&self, timeout: Duration) -> Duration {
        if let Some(fixed) = self.backoff {
            return fixed;
        }
        let divisor = self
            .backoff_divisor
            .unwrap_or(DEFAULT_BACKOFF_DIVISOR)
            .max(1);
        let ceiling =
            (Duration::from_secs(DEFAULT_TIMEOUT_SECS) / divisor).max(CHILD_WAIT_POLL_INTERVAL);
        (timeout / divisor).clamp(CHILD_WAIT_POLL_INTERVAL, ceiling)
    }
}

fn lowercased_markers(markers: &[String]) -> Vec<String> {
    markers
        .iter()
        .map(|marker| marker.trim().to_ascii_lowercase())
        .filter(|marker| !marker.is_empty())
        .collect()
}

/// The policy this process runs with: built-ins plus the user's `[failover]`
/// overrides, resolved once. Classification happens on every failed attempt, so
/// it must not touch the disk each time.
fn effective_policy() -> &'static FailurePolicy {
    static POLICY: OnceLock<FailurePolicy> = OnceLock::new();
    POLICY.get_or_init(|| {
        // Unit tests must not inherit the developer's ~/.rtrt/config.toml —
        // the in-crate test binary always classifies with the shipped
        // defaults, and the override layer is covered by explicit
        // `FailurePolicy::from_config` tests.
        if cfg!(test) {
            FailurePolicy::builtin()
        } else {
            FailurePolicy::from_config(&rtrt_core::Config::load_effective_for_cwd().failover)
        }
    })
}

/// Classify an invocation [`Error`] with the effective policy.
pub fn classify_error(err: &Error) -> FailureClass {
    effective_policy().classify(err)
}

/// Whether a failure leaves the walk anything to try (retry the same target or
/// fall over to the next one).
///
/// Kept as a thin shim over [`classify_error`] so existing callers — the
/// gateway's HTTP status mapping in particular — keep their exact behaviour:
/// the bool is `true` for everything that was retryable before the three-class
/// split (transient *and* quota) and `false` for everything that was terminal
/// ([`FailureClass::Fatal`]).
pub fn is_retryable_error(err: &Error) -> bool {
    classify_error(err).is_recoverable()
}

fn contains_server_status(message: &str) -> bool {
    const SERVER_CONTEXT: &[&str] = &[
        "http",
        "server",
        "service unavailable",
        "status",
        "upstream",
        "overload",
    ];
    if !SERVER_CONTEXT.iter().any(|marker| message.contains(marker)) {
        return false;
    }
    let bytes = message.as_bytes();
    bytes.windows(3).enumerate().any(|(index, code)| {
        code[0] == b'5'
            && code[1].is_ascii_digit()
            && code[2].is_ascii_digit()
            && index
                .checked_sub(1)
                .is_none_or(|before| !bytes[before].is_ascii_digit())
            && bytes
                .get(index + 3)
                .is_none_or(|after| !after.is_ascii_digit())
    })
}

fn classify_label(retryable: bool) -> &'static str {
    if retryable { "retryable" } else { "terminal" }
}

fn target_label(target: &str, model: Option<&str>) -> String {
    match model {
        Some(model) => format!("{target}[{model}]"),
        None => target.to_string(),
    }
}

/// Invoke targets in ranked order under the three-class failure policy.
///
/// Per candidate, the response depends on how its failure classifies:
/// * [`FailureClass::Transient`] — back off briefly (see
///   [`FailurePolicy::backoff`]) and try the **same** target again; only if
///   that also fails does the walk move on.
/// * [`FailureClass::Quota`] — the target is out of allowance, so retrying it
///   would just burn time: fall over to the next lane immediately.
/// * [`FailureClass::Fatal`] — halt. No retry, no failover: a bad credential or
///   a wrong model id cannot be fixed by asking another provider.
///
/// Returns the walk record rather than a bare error, so a caller can report the
/// halted lane. `invoke_agent`'s single-target behaviour is untouched; each
/// underlying call still records to the ledger (failures as `ok = 0`), so
/// balance accounting is identical to a direct invocation.
pub async fn invoke_with_policy(
    targets: &[RankedTarget],
    prompt: &str,
    timeout: Duration,
) -> Result<PolicyOutcome> {
    walk_with_policy(
        effective_policy(),
        targets,
        prompt,
        timeout,
        |candidate, prompt, timeout| async move {
            let opts = InvokeOptions {
                mode: Some(candidate.mode),
                model: candidate.model.clone(),
                timeout,
            };
            invoke_agent(&candidate.target, &prompt, opts).await
        },
    )
    .await
}

/// The policy walk, parameterised over the invoker so tests can drive every
/// class transition without spawning real providers.
async fn walk_with_policy<F, Fut>(
    policy: &FailurePolicy,
    targets: &[RankedTarget],
    prompt: &str,
    timeout: Duration,
    mut invoke: F,
) -> Result<PolicyOutcome>
where
    F: FnMut(RankedTarget, String, Duration) -> Fut,
    Fut: Future<Output = Result<InvokeOutcome>>,
{
    if targets.is_empty() {
        return Err(Error::Provider(
            "invoke: failover received no ranked targets".to_string(),
        ));
    }

    let max_tries = policy.transient_retries().saturating_add(1);
    let mut attempts: Vec<PolicyAttempt> = Vec::new();
    for candidate in targets {
        let mut tries = 0u32;
        loop {
            tries += 1;
            match invoke(candidate.clone(), prompt.to_string(), timeout).await {
                Ok(outcome) => {
                    return Ok(PolicyOutcome {
                        served: Some(outcome),
                        attempts,
                        halted: None,
                    });
                }
                Err(err) => {
                    let class = policy.classify(&err);
                    // Transient: give the same target its retry before
                    // spending another lane's budget on the same request.
                    if class == FailureClass::Transient && tries < max_tries {
                        tokio::time::sleep(policy.backoff(timeout)).await;
                        continue;
                    }
                    let attempt = PolicyAttempt {
                        target: candidate.target.clone(),
                        model: candidate.model.clone(),
                        class,
                        retried: tries > 1,
                        error: err.to_string(),
                    };
                    // Fatal: halt. Falling over would replay a credential or
                    // config mistake against every remaining lane.
                    if class == FailureClass::Fatal {
                        attempts.push(attempt.clone());
                        return Ok(PolicyOutcome {
                            served: None,
                            attempts,
                            halted: Some(attempt),
                        });
                    }
                    // Quota, or a transient that used up its retry: the ledger
                    // already recorded ok=0 inside the invoker; fall over.
                    attempts.push(attempt);
                    break;
                }
            }
        }
    }
    Ok(PolicyOutcome {
        served: None,
        attempts,
        halted: None,
    })
}

/// Invoke targets in ranked order with automatic cross-provider failover.
///
/// A thin flattening of [`invoke_with_policy`] onto the legacy shape: the first
/// success with its failover trail, or an aggregated error listing every
/// attempt (which is also what a fatal halt returns, as it did before the
/// three-class split). Callers that want to report the halted lane should use
/// [`invoke_with_policy`] directly.
pub async fn invoke_with_failover(
    targets: &[RankedTarget],
    prompt: &str,
    timeout: Duration,
) -> Result<FailoverOutcome> {
    invoke_with_policy(targets, prompt, timeout)
        .await?
        .into_failover()
}

/// Build a single error summarizing every failover attempt, in order.
fn aggregated_error(attempts: &[FailoverAttempt]) -> Error {
    let trail = attempts
        .iter()
        .map(|a| {
            format!(
                "{} ({}): {}",
                target_label(&a.target, a.model.as_deref()),
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
            MODEL_ARGS_PLACEHOLDER => {
                if let Some(model) = model {
                    argv.push("--model".to_string());
                    argv.push(model.to_string());
                }
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
        .stdin(Stdio::null())
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
            drain_reader_bounded(stdout_reader).await;
            drain_reader_bounded(stderr_reader).await;
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

fn cli_exit_error(program: &str, exit_code: Option<i32>, output: &str) -> Error {
    let status = match exit_code {
        Some(code) => format!("exited with status {code}"),
        None => "terminated by signal".to_string(),
    };
    let output = sanitize_cli_output(output);
    let detail = if output.is_empty() {
        String::new()
    } else {
        format!(": {output}")
    };
    Error::Provider(format!("invoke: command '{program}' {status}{detail}"))
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

async fn drain_reader_bounded(reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>) {
    let Some(reader) = reader else {
        return;
    };
    let _ = tokio::time::timeout(PIPE_DRAIN_TIMEOUT, reader).await;
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
    fn optional_model_args_expand_to_flag_and_model() {
        let argv = template_to_argv(
            "opencode run {model_args} {prompt}",
            "say hi",
            Some("provider/model"),
        )
        .expect("template should parse");

        assert_eq!(
            argv,
            vec!["opencode", "run", "--model", "provider/model", "say hi"]
        );
    }

    #[test]
    fn optional_model_args_disappear_without_model() {
        let argv = template_to_argv("claude -p {model_args} {prompt}", "say hi", None)
            .expect("template should parse");

        assert_eq!(argv, vec!["claude", "-p", "say hi"]);
    }

    #[test]
    fn required_model_placeholder_still_rejects_missing_model() {
        let err = template_to_argv("ollama run {model} {prompt}", "say hi", None)
            .expect_err("required model should fail");

        assert!(err.to_string().contains("requires --model"));
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
            "openai 529 overloaded",
            "invalid upstream response: 503",
            "gateway: budget exceeded for openai",
            "invoke: command 'ollama' timed out after 120s",
            "invoke: spawn 'codex': No such file or directory",
            "provider overloaded, retry later",
            "daily quota reached",
            "you have hit your weekly usage limit",
            "weekly limit reached",
            "credits exhausted",
            "invoke: target 'claude' is not installed; available targets: opencode",
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
            "invoke: target 'typo' is not detected; available targets: claude",
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
    fn standalone_numbers_are_not_treated_as_http_statuses() {
        assert!(!is_retryable_error(&Error::Provider(
            "configuration requires 512 tokens".to_string()
        )));
    }

    #[test]
    fn terminal_markers_win_over_status_substrings() {
        // A 401 that also mentions "rate limit" must stay terminal: an auth
        // failure will not be fixed by falling over to another provider.
        let err = Error::Provider("anthropic 401: rate limit note".to_string());
        assert!(!is_retryable_error(&err));

        for message in ["model-not-found: 429 rate limit", "invalid api key: 503"] {
            assert!(
                !is_retryable_error(&Error::Provider(message.to_string())),
                "expected terminal for: {message}"
            );
        }
    }

    #[test]
    fn nonzero_exit_error_is_sanitized_and_classified_from_output() {
        let err = cli_exit_error(
            "opencode",
            Some(7),
            "\x1b[31mweekly usage limit reached\x1b[0m",
        );
        let message = err.to_string();

        assert!(message.contains("exited with status 7"));
        assert!(message.contains("weekly usage limit reached"));
        assert!(!message.contains('\x1b'));
        assert!(is_retryable_error(&err));
    }

    #[test]
    fn signal_exit_is_an_error() {
        let err = cli_exit_error("claude", None, "terminated");

        assert!(err.to_string().contains("terminated by signal: terminated"));
    }

    #[tokio::test]
    async fn timeout_pipe_drain_is_bounded() {
        let reader = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(Vec::new())
        });
        let started = Instant::now();

        drain_reader_bounded(Some(reader)).await;

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn failover_stops_on_unknown_target() {
        let targets = vec![
            ranked("__definitely_not_a_real_target__"),
            ranked("__second_unreachable_target__"),
        ];
        let err = invoke_with_failover(&targets, "hi", Duration::from_secs(1))
            .await
            .expect_err("unknown target should be terminal");
        let msg = err.to_string();
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
                model: None,
                retryable: true,
                error: "ollama 429".to_string(),
            }],
        };
        assert_eq!(outcome.fell_over(), 1);
        let summary = outcome.summary();
        assert!(summary.contains("served by openai[gpt-x] after 1 fell over"));
        assert!(summary.contains("ollama: retryable"));
    }

    #[test]
    fn failover_summary_distinguishes_duplicate_targets_by_model() {
        let outcome = FailoverOutcome {
            outcome: InvokeOutcome {
                target: "opencode".to_string(),
                mode_used: Mode::Cli,
                model: Some("provider/model-c".to_string()),
                output: "ok".to_string(),
                exit_code: Some(0),
                ms: 1,
            },
            failed_over: vec![
                FailoverAttempt {
                    target: "opencode".to_string(),
                    model: Some("provider/model-a".to_string()),
                    retryable: true,
                    error: "weekly limit reached".to_string(),
                },
                FailoverAttempt {
                    target: "opencode".to_string(),
                    model: Some("provider/model-b".to_string()),
                    retryable: true,
                    error: "weekly limit reached".to_string(),
                },
            ],
        };

        assert_eq!(
            outcome.summary(),
            "served by opencode[provider/model-c] after 2 fell over (opencode[provider/model-a]: retryable, opencode[provider/model-b]: retryable)"
        );
    }

    #[test]
    fn failover_attempt_deserializes_without_model() {
        let attempt: FailoverAttempt =
            serde_json::from_str(r#"{"target":"opencode","retryable":true,"error":"limited"}"#)
                .expect("legacy attempt should deserialize");

        assert_eq!(attempt.model, None);
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

    // --- three-class failure policy ------------------------------------

    /// The terminal table exactly as it shipped before the split.
    const LEGACY_TERMINAL_MARKERS: &[&str] = &[
        "401",
        "403",
        "unauthorized",
        "forbidden",
        "invalid api key",
        "authentication",
        "api key",
        "not detected",
        "model not found",
        "model-not-found",
        "disabled",
        "does not support",
        "requires --model",
        "needs --model",
        "no cli invocation",
        "is empty",
    ];

    /// The retryable table exactly as it shipped before the split.
    const LEGACY_RETRYABLE_MARKERS: &[&str] = &[
        "429",
        "rate limit",
        "rate-limit",
        "ratelimit",
        "quota",
        "weekly",
        "usage",
        "hit your",
        "limit reached",
        "credits exhausted",
        "overloaded",
        "capacity",
        "too many requests",
        "timed out",
        "timeout",
        "spawn",
        "not installed",
        "budget exceeded",
    ];

    /// The classifier verbatim as it was before the three-class split, so the
    /// `is_retryable_error` shim can be *proven* identical rather than assumed.
    fn legacy_is_retryable_error(err: &Error) -> bool {
        let Error::Provider(message) = err else {
            return true;
        };
        let lower = message.to_ascii_lowercase();
        if LEGACY_TERMINAL_MARKERS.iter().any(|m| lower.contains(m)) {
            return false;
        }
        LEGACY_RETRYABLE_MARKERS.iter().any(|m| lower.contains(m)) || contains_server_status(&lower)
    }

    /// Every marker from both pre-split tables plus the messages the rest of
    /// this module asserts on.
    fn classification_corpus() -> Vec<String> {
        let mut corpus: Vec<String> = LEGACY_TERMINAL_MARKERS
            .iter()
            .chain(LEGACY_RETRYABLE_MARKERS)
            .map(|m| (*m).to_string())
            .collect();
        corpus.extend(
            [
                "anthropic 429: rate limit exceeded",
                "openai 503 Service Unavailable",
                "openai 529 overloaded",
                "invalid upstream response: 503",
                "gateway: budget exceeded for openai",
                "invoke: command 'ollama' timed out after 120s",
                "invoke: spawn 'codex': No such file or directory",
                "provider overloaded, retry later",
                "daily quota reached",
                "you have hit your weekly usage limit",
                "weekly limit reached",
                "credits exhausted",
                "invoke: target 'claude' is not installed; available targets: opencode",
                "anthropic 401: invalid api key",
                "openai 403: forbidden",
                "invoke: target 'typo' is not detected; available targets: claude",
                "invoke: target 'claude' does not support API mode",
                "invoke: target 'openai' API mode requires --model",
                "rtrt call: prompt is empty",
                "configuration requires 512 tokens",
                "anthropic 401: rate limit note",
                "model-not-found: 429 rate limit",
                "invalid api key: 503",
                "weekly usage limit reached",
                "some entirely unrecognised provider message",
            ]
            .iter()
            .map(|m| (*m).to_string()),
        );
        corpus
    }

    #[test]
    fn marker_tables_classify_into_the_intended_failure_classes() {
        let policy = FailurePolicy::builtin();
        for marker in FATAL_MARKERS {
            assert_eq!(
                policy.classify(&Error::Provider((*marker).to_string())),
                FailureClass::Fatal,
                "expected fatal for marker: {marker}"
            );
        }
        for marker in QUOTA_MARKERS {
            assert_eq!(
                policy.classify(&Error::Provider((*marker).to_string())),
                FailureClass::Quota,
                "expected quota for marker: {marker}"
            );
        }
        for marker in TRANSIENT_MARKERS {
            assert_eq!(
                policy.classify(&Error::Provider((*marker).to_string())),
                FailureClass::Transient,
                "expected transient for marker: {marker}"
            );
        }

        // The split is a partition of the pre-split tables: nothing was
        // dropped, invented, or moved between the fatal and recoverable sides.
        assert_eq!(FATAL_MARKERS, LEGACY_TERMINAL_MARKERS);
        assert_eq!(
            QUOTA_MARKERS.len() + TRANSIENT_MARKERS.len(),
            LEGACY_RETRYABLE_MARKERS.len()
        );
        for marker in QUOTA_MARKERS.iter().chain(TRANSIENT_MARKERS) {
            assert!(
                LEGACY_RETRYABLE_MARKERS.contains(marker),
                "{marker} is not a pre-split retryable marker"
            );
        }
    }

    #[test]
    fn shim_verdict_is_identical_to_the_pre_split_classifier() {
        for message in classification_corpus() {
            let err = Error::Provider(message.clone());
            assert_eq!(
                is_retryable_error(&err),
                legacy_is_retryable_error(&err),
                "shim drifted for: {message}"
            );
        }
        // Non-provider errors were retryable before the split; they stay
        // transient (and therefore recoverable) after it.
        let io = Error::Io(std::io::Error::other("socket closed"));
        assert_eq!(classify_error(&io), FailureClass::Transient);
        assert!(is_retryable_error(&io));
        assert!(legacy_is_retryable_error(&io));
    }

    #[test]
    fn absent_failover_config_classifies_exactly_like_the_builtins() {
        let configured = FailurePolicy::from_config(&FailoverConfig::default());
        let builtin = FailurePolicy::builtin();
        for message in classification_corpus() {
            let err = Error::Provider(message.clone());
            assert_eq!(
                configured.classify(&err),
                builtin.classify(&err),
                "default config drifted for: {message}"
            );
        }
        assert_eq!(configured.transient_retries(), builtin.transient_retries());
        assert_eq!(
            configured.backoff(Duration::from_secs(DEFAULT_TIMEOUT_SECS)),
            builtin.backoff(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        );
    }

    #[test]
    fn user_markers_win_over_the_builtin_tables() {
        let policy = FailurePolicy::from_config(&FailoverConfig {
            quota: vec!["  TIMED OUT  ".to_string()],
            fatal: vec!["seat revoked".to_string()],
            transient: vec![String::new()],
            ..FailoverConfig::default()
        });

        // Reclassified built-in marker (and the entry is trimmed/lowercased).
        assert_eq!(
            policy.classify(&Error::Provider(
                "invoke: command 'ollama' timed out after 120s".to_string()
            )),
            FailureClass::Quota
        );
        // A message the built-ins do not know at all.
        assert_eq!(
            policy.classify(&Error::Provider("seat revoked for this org".to_string())),
            FailureClass::Fatal
        );
        // Blank entries are dropped rather than matching every message.
        assert_eq!(
            policy.classify(&Error::Provider("unrecognised message".to_string())),
            FailureClass::Fatal
        );
    }

    #[test]
    fn failover_section_parses_from_config_toml() {
        let config = rtrt_core::Config::from_toml_str(
            "[failover]\nquota = [\"Seat Limit\"]\ntransient_retries = 0\nbackoff_ms = 7\n",
        )
        .expect("failover section should parse");

        assert_eq!(config.failover.quota, vec!["Seat Limit".to_string()]);
        assert_eq!(config.failover.transient_retries, Some(0));

        let policy = FailurePolicy::from_config(&config.failover);
        assert_eq!(policy.transient_retries(), 0);
        assert_eq!(
            policy.backoff(Duration::from_secs(600)),
            Duration::from_millis(7)
        );
        assert_eq!(
            policy.classify(&Error::Provider("provider: SEAT LIMIT hit".to_string())),
            FailureClass::Quota
        );
    }

    #[test]
    fn transient_backoff_is_derived_from_the_call_timeout() {
        let policy = FailurePolicy::builtin();
        let default_timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);

        assert_eq!(
            policy.backoff(default_timeout),
            default_timeout / DEFAULT_BACKOFF_DIVISOR
        );
        // Floor: a tiny budget still pauses for one child-wait poll tick.
        assert_eq!(
            policy.backoff(Duration::from_millis(10)),
            CHILD_WAIT_POLL_INTERVAL
        );
        // Ceiling: an unusually generous budget cannot park the walk.
        assert_eq!(
            policy.backoff(Duration::from_secs(3600)),
            default_timeout / DEFAULT_BACKOFF_DIVISOR
        );
        // A configured divisor still scales with the caller's budget.
        let steeper = FailurePolicy::from_config(&FailoverConfig {
            backoff_divisor: Some(4),
            ..FailoverConfig::default()
        });
        assert_eq!(
            steeper.backoff(Duration::from_secs(8)),
            Duration::from_secs(2)
        );
    }

    /// A scripted invoker: each target gets a list of steps (`None` = success,
    /// `Some(message)` = provider error), the last step repeating forever. It
    /// records the exact call order so a test can assert retries and failover.
    struct Script {
        calls: std::cell::RefCell<Vec<String>>,
        plan: std::collections::BTreeMap<String, Vec<Option<String>>>,
    }

    impl Script {
        fn new(plan: Vec<(&str, Vec<Option<&str>>)>) -> Self {
            Self {
                calls: std::cell::RefCell::new(Vec::new()),
                plan: plan
                    .into_iter()
                    .map(|(target, steps)| {
                        (
                            target.to_string(),
                            steps.into_iter().map(|s| s.map(str::to_string)).collect(),
                        )
                    })
                    .collect(),
            }
        }

        fn next(&self, target: &str) -> Result<InvokeOutcome> {
            self.calls.borrow_mut().push(target.to_string());
            let tries = self
                .calls
                .borrow()
                .iter()
                .filter(|call| call.as_str() == target)
                .count();
            let steps = self.plan.get(target).expect("scripted target");
            match steps[(tries - 1).min(steps.len() - 1)].clone() {
                None => Ok(InvokeOutcome {
                    target: target.to_string(),
                    mode_used: Mode::Cli,
                    model: None,
                    output: "ok".to_string(),
                    exit_code: Some(0),
                    ms: 1,
                }),
                Some(message) => Err(Error::Provider(message)),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    async fn run_walk(
        policy: &FailurePolicy,
        targets: &[RankedTarget],
        script: &Script,
    ) -> Result<PolicyOutcome> {
        walk_with_policy(
            policy,
            targets,
            "hi",
            Duration::from_secs(1),
            |candidate, _prompt, _timeout| {
                let result = script.next(&candidate.target);
                async move { result }
            },
        )
        .await
    }

    #[tokio::test]
    async fn quota_falls_over_immediately_without_retrying_the_same_target() {
        let script = Script::new(vec![
            ("a", vec![Some("anthropic 429: rate limit exceeded")]),
            ("b", vec![None]),
        ]);

        let outcome = run_walk(
            &FailurePolicy::builtin(),
            &[ranked("a"), ranked("b")],
            &script,
        )
        .await
        .expect("walk should complete");

        assert_eq!(script.calls(), vec!["a", "b"]);
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(outcome.attempts[0].class, FailureClass::Quota);
        assert!(!outcome.attempts[0].retried);
        assert!(outcome.halted.is_none());
        assert_eq!(outcome.served.expect("b should serve").target, "b");
    }

    #[tokio::test]
    async fn transient_retries_the_same_target_once_then_falls_over() {
        let script = Script::new(vec![
            ("a", vec![Some("invoke: command 'a' timed out after 1s")]),
            ("b", vec![None]),
        ]);

        let outcome = run_walk(
            &FailurePolicy::builtin(),
            &[ranked("a"), ranked("b")],
            &script,
        )
        .await
        .expect("walk should complete");

        assert_eq!(script.calls(), vec!["a", "a", "b"]);
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(outcome.attempts[0].class, FailureClass::Transient);
        assert!(outcome.attempts[0].retried);
        assert_eq!(outcome.served.expect("b should serve").target, "b");
    }

    #[tokio::test]
    async fn transient_retry_can_succeed_without_falling_over() {
        let script = Script::new(vec![
            ("a", vec![Some("provider overloaded, retry later"), None]),
            ("b", vec![None]),
        ]);

        let outcome = run_walk(
            &FailurePolicy::builtin(),
            &[ranked("a"), ranked("b")],
            &script,
        )
        .await
        .expect("walk should complete");

        assert_eq!(script.calls(), vec!["a", "a"]);
        assert!(outcome.attempts.is_empty());
        assert_eq!(outcome.served.as_ref().expect("a should serve").target, "a");
        assert_eq!(outcome.summary(), "served by a (no failover)");
    }

    #[tokio::test]
    async fn fatal_halts_the_walk_and_never_tries_the_next_target() {
        let script = Script::new(vec![
            ("a", vec![Some("anthropic 401: invalid api key")]),
            ("b", vec![None]),
        ]);

        let outcome = run_walk(
            &FailurePolicy::builtin(),
            &[ranked("a"), ranked("b")],
            &script,
        )
        .await
        .expect("walk should complete");

        assert_eq!(script.calls(), vec!["a"]);
        assert!(outcome.served.is_none());
        assert_eq!(outcome.fell_over(), 0);
        let halted = outcome.halted.clone().expect("halt should be recorded");
        assert_eq!(halted.target, "a");
        assert_eq!(halted.class, FailureClass::Fatal);
        assert!(
            outcome
                .summary()
                .starts_with("halted at a after 0 fell over"),
            "got: {}",
            outcome.summary()
        );

        // Flattened to the legacy shape it is the same aggregated error a
        // terminal failure produced before the split.
        let err = outcome.into_failover().expect_err("a halt is an error");
        let message = err.to_string();
        assert!(
            message.contains("all 1 candidate(s) failed"),
            "got: {message}"
        );
        assert!(message.contains("a (terminal)"), "got: {message}");
    }

    #[tokio::test]
    async fn quota_then_transient_records_both_classes_in_the_trail() {
        let script = Script::new(vec![
            ("a", vec![Some("weekly limit reached")]),
            ("b", vec![Some("invoke: command 'b' timed out after 1s")]),
            ("c", vec![None]),
        ]);

        let outcome = run_walk(
            &FailurePolicy::builtin(),
            &[ranked("a"), ranked("b"), ranked("c")],
            &script,
        )
        .await
        .expect("walk should complete");

        assert_eq!(script.calls(), vec!["a", "b", "b", "c"]);
        assert_eq!(
            outcome.summary(),
            "served by c after 2 fell over (a: quota, b: transient, retried)"
        );
    }

    #[tokio::test]
    async fn config_override_reclassifies_a_transient_marker_as_quota() {
        let policy = FailurePolicy::from_config(&FailoverConfig {
            quota: vec!["timed out".to_string()],
            ..FailoverConfig::default()
        });
        let script = Script::new(vec![
            ("a", vec![Some("invoke: command 'a' timed out after 1s")]),
            ("b", vec![None]),
        ]);

        let outcome = run_walk(&policy, &[ranked("a"), ranked("b")], &script)
            .await
            .expect("walk should complete");

        // No same-target retry: the override moved this marker to quota.
        assert_eq!(script.calls(), vec!["a", "b"]);
        assert_eq!(outcome.attempts[0].class, FailureClass::Quota);
        assert!(!outcome.attempts[0].retried);
    }

    #[tokio::test]
    async fn configured_zero_retries_disables_the_same_target_retry() {
        let policy = FailurePolicy::from_config(&FailoverConfig {
            transient_retries: Some(0),
            ..FailoverConfig::default()
        });
        let script = Script::new(vec![
            ("a", vec![Some("invoke: command 'a' timed out after 1s")]),
            ("b", vec![None]),
        ]);

        let outcome = run_walk(&policy, &[ranked("a"), ranked("b")], &script)
            .await
            .expect("walk should complete");

        assert_eq!(script.calls(), vec!["a", "b"]);
        assert_eq!(outcome.attempts[0].class, FailureClass::Transient);
        assert!(!outcome.attempts[0].retried);
    }

    #[tokio::test]
    async fn exhausted_walk_reports_every_candidate() {
        let script = Script::new(vec![
            ("a", vec![Some("anthropic 429: rate limit exceeded")]),
            ("b", vec![Some("weekly limit reached")]),
        ]);

        let outcome = run_walk(
            &FailurePolicy::builtin(),
            &[ranked("a"), ranked("b")],
            &script,
        )
        .await
        .expect("walk should complete");

        assert!(outcome.served.is_none());
        assert!(outcome.halted.is_none());
        assert_eq!(outcome.fell_over(), 2);
        assert_eq!(
            outcome.summary(),
            "all 2 candidate(s) failed (a: quota, b: quota)"
        );
        let err = outcome.into_failover().expect_err("no candidate served");
        assert!(err.to_string().contains("all 2 candidate(s) failed"));
    }

    #[test]
    fn policy_attempt_flattens_to_the_legacy_attempt_shape() {
        let quota = PolicyAttempt {
            target: "opencode".to_string(),
            model: Some("provider/model".to_string()),
            class: FailureClass::Quota,
            retried: false,
            error: "weekly limit reached".to_string(),
        };
        let fatal = PolicyAttempt {
            class: FailureClass::Fatal,
            ..quota.clone()
        };

        assert!(FailoverAttempt::from(&quota).retryable);
        assert!(!FailoverAttempt::from(&fatal).retryable);
        assert_eq!(FailoverAttempt::from(&quota).model, quota.model);
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
