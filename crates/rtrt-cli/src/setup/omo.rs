//! OmO native MCP configuration and owned extension installation.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    SetupPlan, apply_jsonc_object_diff, backup_if_needed, backup_path, dirs_home,
    parse_json_or_jsonc, path_metadata, write_private_file_atomic_same_dir,
};

const RUNTIME: &str = include_str!("../../../../plugins/omo/rtrt.mjs");
const ENTRY_PREFIX: &str = "// rtrt-managed omo-native v1\n\
import { createRtrtExtension } from \"./runtime.mjs\"\nconst config = ";
const ENTRY_SUFFIX: &str = "\nexport default createRtrtExtension(config)\n";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NativeConfig {
    binary: PathBuf,
    mcp_binary: PathBuf,
    tool_prefix: String,
}

impl NativeConfig {
    fn server(&self) -> Value {
        json!({"type": "stdio", "command": self.mcp_binary, "args": []})
    }
}

pub(super) fn install(plan: &SetupPlan) -> Result<()> {
    update(plan.apply, Some(plan))
}

pub(super) fn uninstall(apply: bool) -> Result<()> {
    update(apply, None)
}

fn agent_dir() -> Result<PathBuf> {
    for name in [
        "OMO_CODING_AGENT_DIR",
        "SENPI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_DIR",
    ] {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("{name} must be an absolute OmO native agent directory");
            }
            return Ok(path);
        }
    }
    Ok(dirs_home()?.join(".omo/agent"))
}

fn read_file(path: &Path) -> Result<Option<String>> {
    for parent in path.ancestors().skip(1) {
        if let Some(metadata) = path_metadata(parent)?
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            bail!(
                "{}: unsafe OmO native directory component",
                parent.display()
            );
        }
    }
    let Some(metadata) = path_metadata(path)? else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{}: refusing non-file or symlink OmO destination",
            path.display()
        );
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))
        .map(Some)
}

fn owned_config(source: &str) -> Result<NativeConfig> {
    let raw = source
        .strip_prefix(ENTRY_PREFIX)
        .and_then(|source| source.strip_suffix(ENTRY_SUFFIX))
        .context("unrecognized or modified OmO native extension; refusing to overwrite")?;
    let config: NativeConfig =
        serde_json::from_str(raw).context("invalid owned OmO native extension configuration")?;
    if !config.binary.is_absolute()
        || !config.mcp_binary.is_absolute()
        || config.tool_prefix.is_empty()
    {
        bail!("invalid owned OmO native executable or tool prefix");
    }
    Ok(config)
}

fn update(apply: bool, plan: Option<&SetupPlan>) -> Result<()> {
    let root = agent_dir()?;
    let mcp_path = root.join("mcp.json");
    let extension_dir = root.join("extensions/rtrt");
    let entry_path = extension_dir.join("index.js");
    let runtime_path = extension_dir.join("runtime.mjs");
    let raw = read_file(&mcp_path)?.unwrap_or_else(|| "{}\n".into());
    let before = parse_json_or_jsonc(&raw, &mcp_path)?;
    if !before.is_object() {
        bail!("{}: OmO MCP config must be an object", mcp_path.display());
    }
    if before
        .get("mcpServers")
        .is_some_and(|value| !value.is_object())
    {
        bail!("{}: mcpServers must be an object", mcp_path.display());
    }
    let entry = read_file(&entry_path)?;
    let previous = entry.as_deref().map(owned_config).transpose()?;
    if plan.is_none() && previous.is_none() {
        println!("{}: no owned OmO native integration", root.display());
        return Ok(());
    }
    let runtime = read_file(&runtime_path)?;
    if runtime.as_deref().is_some_and(|source| source != RUNTIME) {
        bail!(
            "{}: modified or foreign OmO native runtime",
            runtime_path.display()
        );
    }
    let next = match plan {
        Some(plan) => {
            if !plan.binary.is_absolute() {
                bail!("OmO native --binary must be an absolute rtrt-mcp path");
            }
            if apply && !std::fs::metadata(&plan.binary)?.is_file() {
                bail!("OmO native MCP executable is not a file");
            }
            let tool_prefix = match before.pointer("/settings/toolPrefix") {
                None => "mcp",
                Some(Value::String(prefix)) if !prefix.is_empty() => prefix,
                Some(_) => bail!("OmO MCP settings.toolPrefix must be a nonempty string"),
            };
            Some(NativeConfig {
                binary: std::env::current_exe().context("resolve installed rtrt executable")?,
                mcp_binary: plan.binary.clone(),
                tool_prefix: tool_prefix.into(),
            })
        }
        None => None,
    };
    let current = before.pointer("/mcpServers/rtrt");
    let previous_server = previous.as_ref().map(NativeConfig::server);
    let next_server = next.as_ref().map(NativeConfig::server);
    if let Some(current) = current
        && Some(current) != previous_server.as_ref()
        && Some(current) != next_server.as_ref()
    {
        bail!(
            "{}: foreign or modified mcpServers.rtrt; preserving it",
            mcp_path.display()
        );
    }
    let mut after = before.clone();
    match &next_server {
        Some(server) => {
            let object = after.as_object_mut().context("OmO MCP config object")?;
            let servers = object.entry("mcpServers").or_insert_with(|| json!({}));
            servers
                .as_object_mut()
                .context("OmO MCP server map")?
                .insert("rtrt".into(), server.clone());
        }
        None => {
            if let Some(servers) = after.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove("rtrt");
            }
        }
    }
    let rendered = apply_jsonc_object_diff(&raw, &before, &after)?;
    let source = next
        .as_ref()
        .map(|config| {
            serde_json::to_string(config)
                .map(|config| format!("{ENTRY_PREFIX}{config}{ENTRY_SUFFIX}"))
        })
        .transpose()?;
    let backup = backup_path(&mcp_path);
    read_file(&backup)?;
    if !apply {
        println!("[dry-run] OmO native MCP config: {}", mcp_path.display());
        println!("[dry-run] OmO native extension: {}", entry_path.display());
        println!("[dry-run] OmO native runtime: {}", runtime_path.display());
        match &next_server {
            Some(server) => println!(
                "{}",
                serde_json::to_string_pretty(&json!({"mcpServers": {"rtrt": server}}))?
            ),
            None => println!("[dry-run] remove mcpServers.rtrt"),
        }
        return Ok(());
    }
    if before != after && path_metadata(&mcp_path)?.is_some() {
        backup_if_needed(&mcp_path)?;
    }
    if let Some(source) = source {
        if runtime.as_deref() != Some(RUNTIME) {
            write_private_file_atomic_same_dir(&runtime_path, RUNTIME.as_bytes())?;
        }
        if entry.as_deref() != Some(source.as_str()) {
            write_private_file_atomic_same_dir(&entry_path, source.as_bytes())?;
        }
    }
    if before != after {
        write_private_file_atomic_same_dir(&mcp_path, rendered.as_bytes())?;
    }
    if next.is_none() {
        if entry.is_some() {
            std::fs::remove_file(&entry_path).context("remove owned OmO extension")?;
        }
        if runtime.is_some() {
            std::fs::remove_file(&runtime_path).context("remove owned OmO runtime")?;
        }
    }
    println!(
        "OmO native integration updated in {}; /reload or restart OmO.",
        root.display()
    );
    Ok(())
}
