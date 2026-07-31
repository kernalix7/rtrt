use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::Config;

const PATH_ENV_VAR: &str = "PATH";
const PATH_EXTENSION_ENV_VAR: &str = "PATHEXT";
const VERSION_TIMEOUT: Duration = Duration::from_millis(800);
const VERSION_POLL_INTERVAL: Duration = Duration::from_millis(20);
const OLLAMA_HOST: &str = "127.0.0.1";
const OLLAMA_PORT: u16 = 11434;
/// Budget for one runtime round-trip (connect, write, read) against a service
/// that is already listening locally.
const RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_millis(700);
/// Budget for asking a CLI agent to list its models. Unlike ollama's loopback
/// `/api/tags` read this pays a process start (`VERSION_TIMEOUT`, already the
/// measured cost of spawning these CLIs) plus the tool's own round-trips to its
/// provider registry — so the ceiling is derived from those two constants
/// rather than guessed. It is a ceiling, not an expected cost: the probe
/// returns as soon as the child exits.
const MODEL_LIST_ROUND_TRIPS: u32 = 2;
const MODEL_LIST_TIMEOUT: Duration =
    VERSION_TIMEOUT.saturating_add(RUNTIME_PROBE_TIMEOUT.saturating_mul(MODEL_LIST_ROUND_TRIPS));
const HTTP_OK_PREFIX: &str = "HTTP/1.1 200";
/// Characters that may appear inside a `provider/model` identifier. Anything
/// else on a line means it is prose (a header, a warning, a spinner) and not a
/// model id.
const MODEL_ID_PUNCTUATION: &[char] = &['/', '-', '_', '.', ':', '@', '+'];
const ANSI_ESCAPE: char = '\u{1b}';

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolKind {
    CodingAgent,
    LocalRuntime,
    ProviderApi,
    McpServer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationMode {
    Cli,
    Api,
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostClass {
    LocalFree,
    SubscriptionFlat,
    ApiMetered,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Reasoning,
    Code,
    Vision,
    Embed,
    Agentic,
    CheapBulk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedTool {
    pub name: String,
    pub kind: ToolKind,
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    pub invocation_modes: Vec<InvocationMode>,
    pub cli_invocation: Option<String>,
    pub cost_class: CostClass,
    pub capabilities: Vec<Capability>,
    pub config_path: Option<String>,
    pub models: Vec<String>,
    pub server_running: Option<bool>,
    pub enabled: bool,
}

#[derive(Clone, Copy)]
struct ToolDescriptor {
    name: &'static str,
    kind: ToolKind,
    binaries: &'static [&'static str],
    version_args: &'static [&'static str],
    invocation_modes: &'static [InvocationMode],
    cli_invocation: Option<&'static str>,
    cost_class: CostClass,
    capabilities: &'static [Capability],
    config_path: Option<&'static str>,
    env_vars: &'static [&'static str],
    runtime_probe: RuntimeProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeProbe {
    None,
    Ollama,
    /// Enumerate models by running the tool's own list subcommand with these
    /// arguments and reading one model id per stdout line. Adding another
    /// CLI-hosted multi-provider agent is a matter of supplying its list
    /// arguments here — the parser is shared and knows nothing about any
    /// particular tool.
    CliModelList(&'static [&'static str]),
}

#[derive(Default)]
struct DetectionContext {
    path_env: Option<OsString>,
    path_ext_env: Option<OsString>,
    present_env_vars: BTreeSet<String>,
    config: Config,
    home_dir: Option<PathBuf>,
    claude_json: Option<String>,
    codex_toml: Option<String>,
}

const CODING_CAPS: &[Capability] = &[Capability::Reasoning, Capability::Code, Capability::Agentic];
const CODE_AGENT_CAPS: &[Capability] = &[Capability::Code, Capability::Agentic];
const GEMINI_CAPS: &[Capability] = &[
    Capability::Reasoning,
    Capability::Code,
    Capability::Vision,
    Capability::Agentic,
];
const GH_COPILOT_CAPS: &[Capability] = &[Capability::Code];
const OLLAMA_CAPS: &[Capability] = &[Capability::Reasoning, Capability::Code, Capability::Embed];
const LOCAL_REASONING_CODE_CAPS: &[Capability] = &[Capability::Reasoning, Capability::Code];
const LOCAL_VISION_CAPS: &[Capability] =
    &[Capability::Reasoning, Capability::Code, Capability::Vision];
const API_AGENTIC_CAPS: &[Capability] = &[
    Capability::Reasoning,
    Capability::Code,
    Capability::Vision,
    Capability::Agentic,
];
const OPENAI_CAPS: &[Capability] = &[
    Capability::Reasoning,
    Capability::Code,
    Capability::Vision,
    Capability::Embed,
    Capability::CheapBulk,
];
const API_VISION_CAPS: &[Capability] =
    &[Capability::Reasoning, Capability::Code, Capability::Vision];
const API_CHEAP_CAPS: &[Capability] = &[
    Capability::Reasoning,
    Capability::Code,
    Capability::CheapBulk,
];
const API_EMBED_CAPS: &[Capability] = &[Capability::Reasoning, Capability::Code, Capability::Embed];
const API_CODE_CAPS: &[Capability] = &[Capability::Reasoning, Capability::Code];

const CLI_MODE: &[InvocationMode] = &[InvocationMode::Cli];
const API_MODE: &[InvocationMode] = &[InvocationMode::Api];

const EMPTY_BINS: &[&str] = &[];
const VERSION_FLAG: &[&str] = &["--version"];
const OPENCODE_MODELS_ARGS: &[&str] = &["models"];

const REGISTRY: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: "claude",
        kind: ToolKind::CodingAgent,
        binaries: &["claude"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some(
            "claude -p {model_args} {prompt} --allowedTools mcp__rtrt__agent_call",
        ),
        cost_class: CostClass::SubscriptionFlat,
        capabilities: CODING_CAPS,
        config_path: Some("~/.claude.json"),
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "codex",
        kind: ToolKind::CodingAgent,
        binaries: &["codex"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("codex exec {prompt}"),
        cost_class: CostClass::SubscriptionFlat,
        capabilities: CODING_CAPS,
        config_path: Some("~/.codex/config.toml"),
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "opencode",
        kind: ToolKind::CodingAgent,
        binaries: &["opencode"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        // Always use the built-in worker agent. The user's default agent may
        // itself be the RTRT orchestrator, which would recurse back into team
        // dispatch if an internal worker inherited it.
        cli_invocation: Some("opencode run {model_args} --agent build {prompt}"),
        cost_class: CostClass::SubscriptionFlat,
        capabilities: CODING_CAPS,
        config_path: Some("~/.config/opencode"),
        env_vars: &[],
        // One `opencode` target fronts several upstream pools (`opencode-go/…`,
        // `openai/…`, `ollama/…`). Enumerating them is what lets routing bind a
        // lane to a concrete `(target, model)` without hand-written config.
        runtime_probe: RuntimeProbe::CliModelList(OPENCODE_MODELS_ARGS),
    },
    ToolDescriptor {
        name: "aider",
        kind: ToolKind::CodingAgent,
        binaries: &["aider"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("aider {prompt}"),
        cost_class: CostClass::ApiMetered,
        capabilities: CODE_AGENT_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "cursor",
        kind: ToolKind::CodingAgent,
        binaries: &["cursor-agent", "cursor"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("cursor {prompt}"),
        cost_class: CostClass::SubscriptionFlat,
        capabilities: CODE_AGENT_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "gemini",
        kind: ToolKind::CodingAgent,
        binaries: &["gemini"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("gemini {prompt}"),
        cost_class: CostClass::ApiMetered,
        capabilities: GEMINI_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "gh-copilot",
        kind: ToolKind::CodingAgent,
        binaries: &["gh"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("gh copilot suggest {prompt}"),
        cost_class: CostClass::SubscriptionFlat,
        capabilities: GH_COPILOT_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "ollama",
        kind: ToolKind::LocalRuntime,
        binaries: &["ollama"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("ollama run {model} {prompt}"),
        cost_class: CostClass::LocalFree,
        capabilities: OLLAMA_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::Ollama,
    },
    ToolDescriptor {
        name: "llama",
        kind: ToolKind::LocalRuntime,
        binaries: &["llama-server", "llama-cli"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: None,
        cost_class: CostClass::LocalFree,
        capabilities: LOCAL_REASONING_CODE_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "lms",
        kind: ToolKind::LocalRuntime,
        binaries: &["lms"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: None,
        cost_class: CostClass::LocalFree,
        capabilities: LOCAL_VISION_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "jan",
        kind: ToolKind::LocalRuntime,
        binaries: &["jan"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: None,
        cost_class: CostClass::LocalFree,
        capabilities: LOCAL_REASONING_CODE_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "vllm",
        kind: ToolKind::LocalRuntime,
        binaries: &["vllm"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: None,
        cost_class: CostClass::LocalFree,
        capabilities: OLLAMA_CAPS,
        config_path: None,
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "anthropic",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: API_AGENTIC_CAPS,
        config_path: None,
        env_vars: &["ANTHROPIC_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "openai",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: OPENAI_CAPS,
        config_path: None,
        env_vars: &["OPENAI_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "google",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: API_VISION_CAPS,
        config_path: None,
        env_vars: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "openrouter",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: &[
            Capability::Reasoning,
            Capability::Code,
            Capability::Vision,
            Capability::CheapBulk,
        ],
        config_path: None,
        env_vars: &["OPENROUTER_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "groq",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: API_CHEAP_CAPS,
        config_path: None,
        env_vars: &["GROQ_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "mistral",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: API_EMBED_CAPS,
        config_path: None,
        env_vars: &["MISTRAL_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "deepseek",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: API_CHEAP_CAPS,
        config_path: None,
        env_vars: &["DEEPSEEK_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
    ToolDescriptor {
        name: "xai",
        kind: ToolKind::ProviderApi,
        binaries: EMPTY_BINS,
        version_args: &[],
        invocation_modes: API_MODE,
        cli_invocation: None,
        cost_class: CostClass::ApiMetered,
        capabilities: API_CODE_CAPS,
        config_path: None,
        env_vars: &["XAI_API_KEY"],
        runtime_probe: RuntimeProbe::None,
    },
];

pub fn detect_tools() -> Vec<DetectedTool> {
    detect_with_context(DetectionContext::from_system())
}

/// Detect tools while honouring a caller-supplied config for the `enabled`
/// overlay. Used by the CLI to feed the *effective* per-project config
/// (`Config::load_effective(Some(repo))`) so a project's `[agents]` /
/// `[providers]` enable map decides which targets are enabled, instead of the
/// global config that `detect_tools()` loads. Everything else (binary probing,
/// env detection, MCP parsing) is identical.
pub fn detect_tools_with_config(config: Config) -> Vec<DetectedTool> {
    detect_with_context(DetectionContext::from_system_with_config(config))
}

fn detect_with_context(context: DetectionContext) -> Vec<DetectedTool> {
    let context = Arc::new(context);
    let mut handles = Vec::with_capacity(REGISTRY.len());
    for descriptor in REGISTRY {
        let context = Arc::clone(&context);
        handles.push(thread::spawn(move || {
            detect_descriptor(descriptor, &context)
        }));
    }

    let mut tools = Vec::with_capacity(REGISTRY.len());
    for handle in handles {
        if let Ok(tool) = handle.join() {
            tools.push(tool);
        }
    }

    tools.extend(parse_mcp_tools(&context));
    tools
}

pub fn registry_names() -> Vec<&'static str> {
    REGISTRY.iter().map(|descriptor| descriptor.name).collect()
}

fn detect_descriptor(descriptor: &ToolDescriptor, context: &DetectionContext) -> DetectedTool {
    let located = find_first_binary(
        descriptor.binaries,
        context.path_env.as_deref(),
        context.path_ext_env.as_deref(),
    );
    let provider_installed = descriptor
        .env_vars
        .iter()
        .any(|name| context.present_env_vars.contains(*name));
    let installed = located.is_some() || provider_installed;
    let path = located.as_ref().map(|(_, path)| path.display().to_string());
    let version = located
        .as_ref()
        .and_then(|(_, path)| command_version(path, descriptor.version_args));
    let (server_running, models) = match descriptor.runtime_probe {
        RuntimeProbe::Ollama => probe_ollama(),
        probe => (
            None,
            enumerate_cli_models(probe, located.as_ref().map(|(_, path)| path.as_path())),
        ),
    };
    let enabled = enabled_for_descriptor(descriptor, installed, &context.config);

    DetectedTool {
        name: descriptor.name.to_string(),
        kind: descriptor.kind,
        installed,
        path,
        version,
        invocation_modes: descriptor.invocation_modes.to_vec(),
        cli_invocation: descriptor.cli_invocation.map(str::to_string),
        cost_class: descriptor.cost_class,
        capabilities: descriptor.capabilities.to_vec(),
        config_path: descriptor
            .config_path
            .map(|path| expand_home_marker(path, context.home_dir.as_deref())),
        models,
        server_running,
        enabled,
    }
}

fn enabled_for_descriptor(descriptor: &ToolDescriptor, installed: bool, config: &Config) -> bool {
    let override_value = match descriptor.kind {
        ToolKind::ProviderApi => config.providers.enabled_override(descriptor.name),
        ToolKind::CodingAgent => config.agents.enabled_override(descriptor.name),
        ToolKind::LocalRuntime | ToolKind::McpServer => None,
    };
    override_value.unwrap_or(installed)
}

fn expand_home_marker(path: &str, home_dir: Option<&Path>) -> String {
    path.strip_prefix("~/")
        .and_then(|tail| home_dir.map(|home| home.join(tail).display().to_string()))
        .unwrap_or_else(|| path.to_string())
}

fn find_first_binary(
    binaries: &[&str],
    path_env: Option<&std::ffi::OsStr>,
    path_ext_env: Option<&std::ffi::OsStr>,
) -> Option<(String, PathBuf)> {
    binaries.iter().find_map(|binary| {
        find_binary(binary, path_env, path_ext_env).map(|path| ((*binary).to_string(), path))
    })
}

fn find_binary(
    binary: &str,
    path_env: Option<&std::ffi::OsStr>,
    path_ext_env: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let binary_path = Path::new(binary);
    if binary_path.components().count() > 1 && is_executable_candidate(binary_path) {
        return Some(binary_path.to_path_buf());
    }

    let path_env = path_env?;
    for dir in env::split_paths(path_env) {
        for candidate in executable_candidates(&dir, binary, path_ext_env) {
            if is_executable_candidate(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_candidates(
    dir: &Path,
    binary: &str,
    path_ext_env: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    #[cfg(not(windows))]
    {
        let _ = path_ext_env;
        vec![dir.join(binary)]
    }

    #[cfg(windows)]
    {
        let mut candidates = vec![dir.join(binary)];
        if Path::new(binary).extension().is_none() {
            let extensions = path_ext_env
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split(';')
                        .filter(|ext| !ext.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    vec![".COM".into(), ".EXE".into(), ".BAT".into(), ".CMD".into()]
                });
            candidates.extend(
                extensions
                    .into_iter()
                    .map(|ext| dir.join(format!("{binary}{ext}"))),
            );
        }
        candidates
    }
}

fn is_executable_candidate(path: &Path) -> bool {
    path.is_file()
}

fn command_version(path: &Path, version_args: &[&str]) -> Option<String> {
    if version_args.is_empty() {
        return None;
    }
    let mut command = Command::new(path);
    command.args(version_args);
    let output = run_bounded(command, VERSION_TIMEOUT)?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    if text.trim().is_empty() {
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    normalize_version(&text)
}

/// Run a child process to completion under a wall-clock ceiling, killing it if
/// it overruns. Every failure mode — spawn error, timeout, wait error — folds
/// into `None`, because detection is advisory: no probe may abort or stall a
/// `rtrt detect` sweep.
fn run_bounded(mut command: Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => return child.wait_with_output().ok(),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(VERSION_POLL_INTERVAL),
            Err(_) => return None,
        }
    }
}

fn normalize_version(raw: &str) -> Option<String> {
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(120).collect())
}

/// Ask an installed CLI agent to list its own models.
///
/// Returns an empty list unless the probe both enumerates models this way and
/// found the tool's binary: a tool that is not installed is never spawned, so a
/// machine without it sees exactly the detection it saw before.
fn enumerate_cli_models(probe: RuntimeProbe, located: Option<&Path>) -> Vec<String> {
    model_list_command(probe, located)
        .map(|(path, args)| probe_cli_models(path, args))
        .unwrap_or_default()
}

/// The command a runtime probe would run to enumerate models, or `None` when it
/// must not run one. Split out from the spawn so that "never probe a tool that
/// is not installed" is a pure decision — [`probe_cli_models`] is unreachable
/// without a `Some` from here.
fn model_list_command(
    probe: RuntimeProbe,
    located: Option<&Path>,
) -> Option<(&Path, &'static [&'static str])> {
    match probe {
        RuntimeProbe::CliModelList(args) if !args.is_empty() => located.map(|path| (path, args)),
        _ => None,
    }
}

/// Run the list command and parse its stdout. Bounded by [`MODEL_LIST_TIMEOUT`]
/// and non-fatal by construction: a timeout, a non-zero exit, or output that
/// holds no model ids all yield an empty list rather than an error, so a tool
/// whose list command misbehaves degrades to "models unknown" instead of
/// breaking detection.
fn probe_cli_models(path: &Path, args: &[&str]) -> Vec<String> {
    let mut command = Command::new(path);
    command.args(args);
    // Ask for plain output. The parser strips escapes anyway, but a CLI told to
    // stay dumb is less likely to emit spinners or paginate.
    command.env("NO_COLOR", "1").env("TERM", "dumb");
    let Some(output) = run_bounded(command, MODEL_LIST_TIMEOUT) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_cli_model_list(&String::from_utf8_lossy(&output.stdout))
}

/// Parse a `provider/model`-per-line listing.
///
/// Deliberately tool-agnostic and defensive: escape sequences are stripped,
/// blank lines and anything that does not look like a model id are dropped, and
/// duplicates are removed while first-seen order is preserved. Order matters —
/// the router picks the first model a target offers.
fn parse_cli_model_list(raw: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for line in raw.lines() {
        let line = strip_ansi(line);
        let candidate = line.trim();
        if !looks_like_model_id(candidate) {
            continue;
        }
        if seen.insert(candidate.to_string()) {
            models.push(candidate.to_string());
        }
    }
    models
}

/// A model id is `provider/model` (occasionally with more segments, as when a
/// gateway re-exports another provider's namespace). Every segment must be
/// non-empty and start alphanumerically, and the whole id must be free of
/// whitespace and prose punctuation.
fn looks_like_model_id(candidate: &str) -> bool {
    if !candidate.contains('/') {
        return false;
    }
    if !candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || MODEL_ID_PUNCTUATION.contains(&c))
    {
        return false;
    }
    candidate.split('/').all(|segment| {
        segment
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Drop ANSI escape sequences (`ESC` followed by parameter bytes and an
/// alphabetic final byte) so a colourised listing still parses.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != ANSI_ESCAPE {
            out.push(c);
            continue;
        }
        for next in chars.by_ref() {
            if next.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

fn probe_ollama() -> (Option<bool>, Vec<String>) {
    let Ok(mut addrs) = (OLLAMA_HOST, OLLAMA_PORT).to_socket_addrs() else {
        return (Some(false), Vec::new());
    };
    let Some(addr) = addrs.next() else {
        return (Some(false), Vec::new());
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, RUNTIME_PROBE_TIMEOUT) else {
        return (Some(false), Vec::new());
    };
    let _ = stream.set_read_timeout(Some(RUNTIME_PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(RUNTIME_PROBE_TIMEOUT));
    let request = format!(
        "GET /api/tags HTTP/1.1\r\nHost: {OLLAMA_HOST}:{OLLAMA_PORT}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return (Some(true), Vec::new());
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return (Some(true), Vec::new());
    }
    let models = parse_ollama_models(&response);
    (Some(response.starts_with(HTTP_OK_PREFIX)), models)
}

fn parse_ollama_models(response: &str) -> Vec<String> {
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or(response);
    // Ollama replies with HTTP/1.1 chunked framing (a hex chunk-size line
    // precedes the JSON and a `0` terminator follows), so the raw body is not
    // valid JSON. The /api/tags object fits in a single chunk, so parse the
    // span from the first `{` to the last `}`.
    let json = match (body.find('{'), body.rfind('}')) {
        (Some(start), Some(end)) if end >= start => &body[start..=end],
        _ => return Vec::new(),
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    value
        .get("models")
        .and_then(|models| models.as_array())
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model.get("name").and_then(|name| name.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_mcp_tools(context: &DetectionContext) -> Vec<DetectedTool> {
    let mut tools = Vec::new();
    if let Some(raw) = &context.claude_json {
        tools.extend(
            parse_claude_mcp_servers(raw)
                .into_iter()
                .map(|server| mcp_tool(server, "~/.claude.json", context.home_dir.as_deref())),
        );
    }
    if let Some(raw) = &context.codex_toml {
        tools.extend(
            parse_codex_mcp_servers(raw).into_iter().map(|server| {
                mcp_tool(server, "~/.codex/config.toml", context.home_dir.as_deref())
            }),
        );
    }
    tools
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpServer {
    name: String,
    command: String,
}

fn mcp_tool(server: McpServer, config_path: &str, home_dir: Option<&Path>) -> DetectedTool {
    DetectedTool {
        name: server.name,
        kind: ToolKind::McpServer,
        installed: true,
        path: Some(server.command.clone()),
        version: None,
        invocation_modes: vec![InvocationMode::Mcp],
        cli_invocation: Some(server.command),
        cost_class: CostClass::Unknown,
        capabilities: Vec::new(),
        config_path: Some(expand_home_marker(config_path, home_dir)),
        models: Vec::new(),
        server_running: None,
        enabled: true,
    }
}

fn parse_claude_mcp_servers(raw: &str) -> Vec<McpServer> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    value
        .get("mcpServers")
        .and_then(|servers| servers.as_object())
        .map(|servers| {
            servers
                .iter()
                .filter_map(|(name, server)| {
                    server
                        .get("command")
                        .and_then(|command| command.as_str())
                        .filter(|command| !command.is_empty())
                        .map(|command| McpServer {
                            name: name.clone(),
                            command: command.to_string(),
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_codex_mcp_servers(raw: &str) -> Vec<McpServer> {
    let Ok(value) = raw.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(servers) = value
        .get("mcp_servers")
        .and_then(|servers| servers.as_table())
    else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, server)| {
            server
                .get("command")
                .and_then(|command| command.as_str())
                .filter(|command| !command.is_empty())
                .map(|command| McpServer {
                    name: name.clone(),
                    command: command.to_string(),
                })
        })
        .collect()
}

impl DetectionContext {
    fn from_system() -> Self {
        Self::from_system_with_config(Config::load().unwrap_or_default())
    }

    /// Same system probe as [`Self::from_system`] but with a caller-supplied
    /// config so the `enabled` overlay can reflect the effective per-project
    /// config instead of the global one.
    fn from_system_with_config(config: Config) -> Self {
        let home_dir = dirs::home_dir();
        let claude_json = read_home_file(home_dir.as_deref(), ".claude.json");
        let codex_toml = read_home_file(home_dir.as_deref(), ".codex/config.toml");
        Self {
            path_env: env::var_os(PATH_ENV_VAR),
            path_ext_env: env::var_os(PATH_EXTENSION_ENV_VAR),
            present_env_vars: env::vars()
                .filter_map(|(name, value)| (!value.is_empty()).then_some(name))
                .collect(),
            config,
            home_dir,
            claude_json,
            codex_toml,
        }
    }
}

fn read_home_file(home_dir: Option<&Path>, relative: &str) -> Option<String> {
    home_dir
        .map(|home| home.join(relative))
        .and_then(|path| fs::read_to_string(path).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_contains_expected_targets() {
        let names = registry_names();
        for expected in [
            "claude",
            "codex",
            "opencode",
            "aider",
            "cursor",
            "gemini",
            "gh-copilot",
            "ollama",
            "llama",
            "lms",
            "jan",
            "vllm",
            "anthropic",
            "openai",
            "google",
            "openrouter",
            "groq",
            "mistral",
            "deepseek",
            "xai",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn cli_templates_forward_optional_models_where_supported() {
        let template = |name| {
            REGISTRY
                .iter()
                .find(|descriptor| descriptor.name == name)
                .and_then(|descriptor| descriptor.cli_invocation)
        };

        assert_eq!(
            template("claude"),
            Some("claude -p {model_args} {prompt} --allowedTools mcp__rtrt__agent_call")
        );
        assert_eq!(
            template("opencode"),
            Some("opencode run {model_args} --agent build {prompt}")
        );
        assert_eq!(template("ollama"), Some("ollama run {model} {prompt}"));
    }

    #[test]
    fn provider_detection_uses_env_presence_without_value() {
        let descriptor = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "openrouter")
            .unwrap();
        let context = DetectionContext {
            present_env_vars: BTreeSet::from(["OPENROUTER_API_KEY".to_string()]),
            config: Config::default(),
            ..DetectionContext::default()
        };
        let tool = detect_descriptor(descriptor, &context);
        assert!(tool.installed);
        assert_eq!(tool.path, None);
        assert!(tool.enabled);
    }

    #[test]
    fn enabled_defaults_to_installed_and_honours_opt_outs() {
        let config = Config::from_toml_str(
            r#"
            [agents]
            claude = false
            aider = true

            [providers]
            active = "openai"
            openrouter = false
            "#,
        )
        .unwrap();
        let claude = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "claude")
            .unwrap();
        let aider = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "aider")
            .unwrap();
        let openrouter = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "openrouter")
            .unwrap();
        let ollama = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "ollama")
            .unwrap();

        assert!(!enabled_for_descriptor(claude, true, &config));
        assert!(enabled_for_descriptor(aider, false, &config));
        assert!(!enabled_for_descriptor(openrouter, true, &config));
        assert!(enabled_for_descriptor(ollama, true, &config));
        assert!(!enabled_for_descriptor(ollama, false, &config));
    }

    #[test]
    fn parses_claude_mcp_servers() {
        let servers = parse_claude_mcp_servers(
            r#"{
                "mcpServers": {
                    "rtrt": { "command": "rtrt-mcp" },
                    "missing": { "args": [] }
                }
            }"#,
        );
        assert_eq!(
            servers,
            vec![McpServer {
                name: "rtrt".to_string(),
                command: "rtrt-mcp".to_string(),
            }]
        );
    }

    #[test]
    fn parses_codex_mcp_servers() {
        let servers = parse_codex_mcp_servers(
            r#"
            [mcp_servers.rtrt]
            command = "rtrt-mcp"

            [mcp_servers.empty]
            args = []
            "#,
        );
        assert_eq!(
            servers,
            vec![McpServer {
                name: "rtrt".to_string(),
                command: "rtrt-mcp".to_string(),
            }]
        );
    }

    /// Captured from a real `opencode models` run, with a blank line, a
    /// duplicate, a colourised line and two junk lines spliced in.
    const OPENCODE_MODELS_SAMPLE: &str = concat!(
        "opencode-go/glm-5.2\n",
        "\n",
        "opencode-go/kimi-k2.7-code\n",
        "opencode-go/kimi-k3\n",
        "Available models:\n",
        "openai/gpt-5.6-sol\n",
        "\u{1b}[32mopenai/gpt-5.6-terra\u{1b}[0m\n",
        "openai/gpt-5.6-luna\n",
        "   \n",
        "ollama/glm-5.2:cloud\n",
        "ollama/kimi-k2.7-code:cloud\n",
        "ollama/kimi-k3:cloud\n",
        "opencode-go/glm-5.2\n",
        "  * pick one with --model\n",
    );

    #[test]
    fn parses_cli_model_list_from_captured_output() {
        assert_eq!(
            parse_cli_model_list(OPENCODE_MODELS_SAMPLE),
            vec![
                "opencode-go/glm-5.2",
                "opencode-go/kimi-k2.7-code",
                "opencode-go/kimi-k3",
                "openai/gpt-5.6-sol",
                "openai/gpt-5.6-terra",
                "openai/gpt-5.6-luna",
                "ollama/glm-5.2:cloud",
                "ollama/kimi-k2.7-code:cloud",
                "ollama/kimi-k3:cloud",
            ]
        );
    }

    #[test]
    fn cli_model_list_rejects_empty_and_junk_output() {
        for raw in [
            "",
            "\n\n   \n",
            "no models configured\n",
            "error: not logged in\n",
            "/leading-slash\n",
            "trailing-slash/\n",
            "provider//model\n",
            "opencode-go/glm 5.2\n",
            "==============\n",
        ] {
            assert!(
                parse_cli_model_list(raw).is_empty(),
                "expected no models from {raw:?}"
            );
        }
    }

    #[test]
    fn cli_model_list_keeps_pool_distinct_ids_that_share_a_model_name() {
        // `PoolKey` is the segment before the first `/`, so the same model
        // reached through two pools must survive as two entries.
        assert_eq!(
            parse_cli_model_list("opencode-go/gpt-5.6-luna\nopenai/gpt-5.6-luna\n"),
            vec!["opencode-go/gpt-5.6-luna", "openai/gpt-5.6-luna"]
        );
    }

    #[test]
    fn model_list_probe_never_runs_for_a_missing_binary() {
        let probe = RuntimeProbe::CliModelList(OPENCODE_MODELS_ARGS);
        let path = Path::new("/nonexistent/bin/opencode");

        // No located binary means no command at all — the spawn is unreachable.
        assert!(model_list_command(probe, None).is_none());
        assert!(enumerate_cli_models(probe, None).is_empty());

        // Probes that do not enumerate this way never produce a command either.
        assert!(model_list_command(RuntimeProbe::None, Some(path)).is_none());
        assert!(model_list_command(RuntimeProbe::Ollama, Some(path)).is_none());
        assert!(model_list_command(RuntimeProbe::CliModelList(&[]), Some(path)).is_none());

        assert_eq!(
            model_list_command(probe, Some(path)),
            Some((path, OPENCODE_MODELS_ARGS))
        );
    }

    #[test]
    fn uninstalled_cli_agent_reports_no_models() {
        let descriptor = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "opencode")
            .unwrap();
        let tool = detect_descriptor(descriptor, &DetectionContext::default());
        assert!(!tool.installed);
        assert_eq!(tool.path, None);
        assert!(tool.models.is_empty());
        assert_eq!(tool.server_running, None);
    }

    #[test]
    fn opencode_enumerates_models_through_its_own_list_command() {
        let descriptor = REGISTRY
            .iter()
            .find(|descriptor| descriptor.name == "opencode")
            .unwrap();
        assert_eq!(
            descriptor.runtime_probe,
            RuntimeProbe::CliModelList(&["models"])
        );
    }

    #[test]
    fn model_list_budget_covers_a_process_start_plus_its_round_trips() {
        assert!(MODEL_LIST_TIMEOUT > VERSION_TIMEOUT);
        assert!(MODEL_LIST_TIMEOUT > RUNTIME_PROBE_TIMEOUT);
        assert_eq!(
            MODEL_LIST_TIMEOUT,
            VERSION_TIMEOUT + RUNTIME_PROBE_TIMEOUT * MODEL_LIST_ROUND_TRIPS
        );
    }

    #[test]
    fn parses_ollama_models_from_http_body() {
        let models = parse_ollama_models(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"models\":[{\"name\":\"llama3.2\"},{\"name\":\"bge-m3\"}]}",
        );
        assert_eq!(models, vec!["llama3.2", "bge-m3"]);
    }
}
