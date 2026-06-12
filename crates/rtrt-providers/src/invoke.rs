use std::{
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use rtrt_core::{CostClass, DetectedTool, Error, InvocationMode, Result};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::{ChatMessage, ChatRequest, Gateway, Role};

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

const CHILD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const API_MAX_TOKENS: u32 = 1024;
const PROMPT_PLACEHOLDER: &str = "{prompt}";
const MODEL_PLACEHOLDER: &str = "{model}";

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

    let (output, exit_code) = match mode_used {
        Mode::Cli => {
            let template = tool.cli_invocation.as_deref().ok_or_else(|| {
                Error::Provider(format!(
                    "invoke: target '{}' has no CLI invocation",
                    tool.name
                ))
            })?;
            let argv = template_to_argv(template, prompt, model.as_deref())?;
            run_cli_argv(&argv, opts.timeout).await?
        }
        Mode::Api => {
            let model = model.as_deref().ok_or_else(|| {
                Error::Provider(format!(
                    "invoke: target '{}' API mode requires --model",
                    tool.name
                ))
            })?;
            let req = ChatRequest {
                model: model.to_string(),
                messages: vec![ChatMessage {
                    role: Role::User,
                    content: prompt.to_string(),
                }],
                max_tokens: Some(API_MAX_TOKENS),
                temperature: None,
            };
            let resp = Gateway::from_env().chat(req).await?;
            (resp.content, None)
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
    Ok((output, status.code()))
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
