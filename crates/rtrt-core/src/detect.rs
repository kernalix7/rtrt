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
const OLLAMA_HTTP_TIMEOUT: Duration = Duration::from_millis(700);
const HTTP_OK_PREFIX: &str = "HTTP/1.1 200";

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeProbe {
    None,
    Ollama,
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

const REGISTRY: &[ToolDescriptor] = &[
    ToolDescriptor {
        name: "claude",
        kind: ToolKind::CodingAgent,
        binaries: &["claude"],
        version_args: VERSION_FLAG,
        invocation_modes: CLI_MODE,
        cli_invocation: Some("claude -p {prompt}"),
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
        cli_invocation: Some("opencode run {prompt}"),
        cost_class: CostClass::SubscriptionFlat,
        capabilities: CODING_CAPS,
        config_path: Some("~/.config/opencode"),
        env_vars: &[],
        runtime_probe: RuntimeProbe::None,
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
    let context = Arc::new(DetectionContext::from_system());
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
        RuntimeProbe::None => (None, Vec::new()),
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
    let mut child = Command::new(path)
        .args(version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let output = child.wait_with_output().ok()?;
                let mut text = String::new();
                text.push_str(&String::from_utf8_lossy(&output.stdout));
                if text.trim().is_empty() {
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                return normalize_version(&text);
            }
            Ok(None) if started.elapsed() >= VERSION_TIMEOUT => {
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

fn probe_ollama() -> (Option<bool>, Vec<String>) {
    let Ok(mut addrs) = (OLLAMA_HOST, OLLAMA_PORT).to_socket_addrs() else {
        return (Some(false), Vec::new());
    };
    let Some(addr) = addrs.next() else {
        return (Some(false), Vec::new());
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, OLLAMA_HTTP_TIMEOUT) else {
        return (Some(false), Vec::new());
    };
    let _ = stream.set_read_timeout(Some(OLLAMA_HTTP_TIMEOUT));
    let _ = stream.set_write_timeout(Some(OLLAMA_HTTP_TIMEOUT));
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
        let home_dir = dirs::home_dir();
        let claude_json = read_home_file(home_dir.as_deref(), ".claude.json");
        let codex_toml = read_home_file(home_dir.as_deref(), ".codex/config.toml");
        Self {
            path_env: env::var_os(PATH_ENV_VAR),
            path_ext_env: env::var_os(PATH_EXTENSION_ENV_VAR),
            present_env_vars: env::vars()
                .filter_map(|(name, value)| (!value.is_empty()).then_some(name))
                .collect(),
            config: Config::load().unwrap_or_default(),
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

    #[test]
    fn parses_ollama_models_from_http_body() {
        let models = parse_ollama_models(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"models\":[{\"name\":\"llama3.2\"},{\"name\":\"bge-m3\"}]}",
        );
        assert_eq!(models, vec!["llama3.2", "bge-m3"]);
    }
}
