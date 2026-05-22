//! `rtrt setup --agent <name>` — wire RTRT into popular coding agents.
//!
//! Supported agents:
//! - `claude`   — `~/.claude.json` (`mcpServers.rtrt`)
//! - `cursor`   — `~/.cursor/mcp.json` (`mcpServers.rtrt`)
//! - `windsurf` — `~/.windsurf/mcp_config.json` (`mcpServers.rtrt`)
//! - `codex`    — `~/.codex/config.toml` (`[mcp_servers.rtrt]`)
//! - `aider`    — prints env-var hint; aider has no MCP config file.
//!
//! Default behaviour is **dry-run**: print the path + snippet so the user can
//! review. Pass `--apply` to write the merged config. A `.bak` is written
//! alongside the original on first apply.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AgentKind {
    Claude,
    Cursor,
    Windsurf,
    Codex,
    Aider,
}

pub struct SetupPlan {
    pub agent: AgentKind,
    pub apply: bool,
    pub memory_path: Option<PathBuf>,
    pub binary: PathBuf,
    pub plugin: bool,
}

/// Files that make up the bundled Claude Code plugin. Each entry is
/// `(relative_path, contents, executable)`. Sourced at compile time from
/// `plugins/claude-code/rtrt/` via `include_str!` so the binary needs no
/// network or repo checkout to install the plugin.
const PLUGIN_FILES: &[(&str, &str, bool)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../../../plugins/claude-code/rtrt/.claude-plugin/plugin.json"),
        false,
    ),
    (
        "README.md",
        include_str!("../../../plugins/claude-code/rtrt/README.md"),
        false,
    ),
    (
        "hooks/_common.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/_common.sh"),
        true,
    ),
    (
        "hooks/pre_tool_use.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/pre_tool_use.sh"),
        true,
    ),
    (
        "hooks/post_tool_use.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/post_tool_use.sh"),
        true,
    ),
    (
        "hooks/post_tool_use_failure.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/post_tool_use_failure.sh"),
        true,
    ),
    (
        "hooks/pre_compact.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/pre_compact.sh"),
        true,
    ),
    (
        "hooks/user_prompt_submit.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/user_prompt_submit.sh"),
        true,
    ),
    (
        "hooks/post_user_prompt_submit.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/post_user_prompt_submit.sh"),
        true,
    ),
    (
        "hooks/notification.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/notification.sh"),
        true,
    ),
    (
        "hooks/stop.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/stop.sh"),
        true,
    ),
    (
        "hooks/subagent_start.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/subagent_start.sh"),
        true,
    ),
    (
        "hooks/subagent_stop.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/subagent_stop.sh"),
        true,
    ),
    (
        "hooks/session_start.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/session_start.sh"),
        true,
    ),
    (
        "hooks/session_end.sh",
        include_str!("../../../plugins/claude-code/rtrt/hooks/session_end.sh"),
        true,
    ),
];

pub fn run(plan: SetupPlan) -> Result<()> {
    let binary = plan.binary.to_string_lossy().to_string();
    // Default to ~/.rtrt/memory.sqlite (absolute) so the MCP server, the
    // plugin hooks (via `_common.sh`'s same default), and any ad-hoc CLI
    // invocation all read and write the same SQLite file. Without this the
    // MCP server falls back to a cwd-relative path and ends up on a
    // different store than the rest of the toolchain.
    let memory_path = Some(plan.memory_path.clone().unwrap_or_else(|| {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".rtrt").join("memory.sqlite")
    }));
    if plan.plugin && !matches!(plan.agent, AgentKind::Claude) {
        bail!("--plugin is only valid with --agent claude");
    }
    if plan.plugin {
        install_claude_plugin(plan.apply)?;
    }
    match plan.agent {
        AgentKind::Aider => {
            println!(
                "aider has no MCP config file. To use RTRT alongside aider:\n\
                 \n\
                 1. Start the MCP server in a separate shell:\n\
                 \n\
                       {binary} --memory $HOME/.rtrt/memory.sqlite\n\
                 \n\
                 2. Use RTRT's CLI from inside aider (e.g. `/run rtrt compress -l ultra < ...`).\n",
            );
            Ok(())
        }
        AgentKind::Claude => apply_json(&plan, "~/.claude.json", &binary, &memory_path),
        AgentKind::Cursor => apply_json(&plan, "~/.cursor/mcp.json", &binary, &memory_path),
        AgentKind::Windsurf => {
            apply_json(&plan, "~/.windsurf/mcp_config.json", &binary, &memory_path)
        }
        AgentKind::Codex => apply_codex_toml(&plan, &binary, &memory_path),
    }
}

fn apply_json(
    plan: &SetupPlan,
    rel_path: &str,
    binary: &str,
    memory_path: &Option<PathBuf>,
) -> Result<()> {
    let path = expand_home(rel_path)?;
    let snippet = render_json_snippet(binary, memory_path);
    if !plan.apply {
        println!("[dry-run] target: {}", path.display());
        println!("[dry-run] snippet:\n{snippet}");
        println!("\nRe-run with --apply to merge into the file.");
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut root: serde_json::Value = if path.exists() {
        backup_if_needed(&path)?;
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("{}: existing file is not valid JSON", path.display()))?
    } else {
        serde_json::json!({})
    };
    let entry = build_json_entry(binary, memory_path);
    if !root.is_object() {
        bail!("{}: root is not a JSON object", path.display());
    }
    let obj = root.as_object_mut().unwrap();
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        bail!("{}: mcpServers exists but is not an object", path.display());
    }
    servers
        .as_object_mut()
        .unwrap()
        .insert("rtrt".to_string(), entry);
    let rendered = serde_json::to_string_pretty(&root)?;
    std::fs::write(&path, rendered).with_context(|| format!("write {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn apply_codex_toml(plan: &SetupPlan, binary: &str, memory_path: &Option<PathBuf>) -> Result<()> {
    let path = expand_home("~/.codex/config.toml")?;
    let snippet = render_codex_toml_snippet(binary, memory_path);
    if !plan.apply {
        println!("[dry-run] target: {}", path.display());
        println!("[dry-run] snippet (append to file if [mcp_servers.rtrt] not already present):");
        println!("\n{snippet}");
        println!("Re-run with --apply to append.");
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let existing = if path.exists() {
        backup_if_needed(&path)?;
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    if existing.contains("[mcp_servers.rtrt]") {
        println!(
            "{}: [mcp_servers.rtrt] already present; nothing to do",
            path.display()
        );
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&snippet);
    std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
    println!("appended [mcp_servers.rtrt] to {}", path.display());
    Ok(())
}

fn build_json_entry(binary: &str, memory_path: &Option<PathBuf>) -> serde_json::Value {
    let args = match memory_path {
        Some(p) => serde_json::json!(["--memory", p.to_string_lossy()]),
        None => serde_json::json!([]),
    };
    serde_json::json!({
        "command": binary,
        "args": args,
    })
}

fn render_json_snippet(binary: &str, memory_path: &Option<PathBuf>) -> String {
    let entry = build_json_entry(binary, memory_path);
    let wrapped = serde_json::json!({ "mcpServers": { "rtrt": entry } });
    serde_json::to_string_pretty(&wrapped).unwrap_or_else(|_| String::new())
}

fn render_codex_toml_snippet(binary: &str, memory_path: &Option<PathBuf>) -> String {
    let mut out = String::new();
    out.push_str("[mcp_servers.rtrt]\n");
    out.push_str(&format!("command = {:?}\n", binary));
    match memory_path {
        Some(p) => {
            out.push_str(&format!(
                "args = [\"--memory\", {:?}]\n",
                p.to_string_lossy()
            ));
        }
        None => out.push_str("args = []\n"),
    }
    out
}

fn expand_home(rel: &str) -> Result<PathBuf> {
    let home = dirs_home()?;
    if let Some(rest) = rel.strip_prefix("~/") {
        Ok(home.join(rest))
    } else {
        Ok(PathBuf::from(rel))
    }
}

fn dirs_home() -> Result<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(h));
    }
    if let Some(h) = std::env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(h));
    }
    bail!("cannot resolve home dir: neither HOME nor USERPROFILE is set")
}

/// Materialises the bundled Claude Code plugin under
/// `~/.claude/plugins/cache/rtrt/` and adds `"rtrt"` to the `plugins`
/// array in `~/.claude/settings.json`. Dry-run prints the targets;
/// `--apply` does the writes.
fn install_claude_plugin(apply: bool) -> Result<()> {
    let plugin_dir = expand_home("~/.claude/plugins/cache/rtrt")?;
    let settings = expand_home("~/.claude/settings.json")?;
    if !apply {
        println!("[dry-run] plugin dir:   {}", plugin_dir.display());
        println!("[dry-run] settings:     {}", settings.display());
        println!("[dry-run] files:        {} entries", PLUGIN_FILES.len());
        println!("Re-run with --apply to write the plugin tree + enable it.");
        return Ok(());
    }
    std::fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("mkdir {}", plugin_dir.display()))?;
    for (rel, contents, executable) in PLUGIN_FILES {
        let target = plugin_dir.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(&target, contents).with_context(|| format!("write {}", target.display()))?;
        #[cfg(unix)]
        if *executable {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&target, perms)
                .with_context(|| format!("chmod {}", target.display()))?;
        }
        #[cfg(not(unix))]
        let _ = executable;
    }
    println!("plugin written to {}", plugin_dir.display());

    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let mut root: serde_json::Value = if settings.exists() {
        backup_if_needed(&settings)?;
        let raw = std::fs::read_to_string(&settings)
            .with_context(|| format!("read {}", settings.display()))?;
        if raw.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("{}: not valid JSON", settings.display()))?
        }
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        bail!("{}: root is not a JSON object", settings.display());
    }
    let obj = root.as_object_mut().unwrap();
    let plugins = obj
        .entry("plugins")
        .or_insert_with(|| serde_json::json!([]));
    if !plugins.is_array() {
        bail!("{}: plugins exists but is not an array", settings.display());
    }
    let arr = plugins.as_array_mut().unwrap();
    let already = arr.iter().any(|v| v.as_str() == Some("rtrt"));
    if !already {
        arr.push(serde_json::Value::String("rtrt".into()));
    }
    let rendered = serde_json::to_string_pretty(&root)?;
    std::fs::write(&settings, rendered).with_context(|| format!("write {}", settings.display()))?;
    if already {
        println!("settings already lists `rtrt` in plugins; left as-is");
    } else {
        println!("enabled in {}", settings.display());
    }
    Ok(())
}

/// Reverse of `install_claude_plugin`. Removes the embedded plugin tree
/// and drops `"rtrt"` from the settings.json `plugins` array. Best-effort:
/// missing pieces are reported but not fatal.
pub fn uninstall_claude_plugin(apply: bool) -> Result<()> {
    let plugin_dir = expand_home("~/.claude/plugins/cache/rtrt")?;
    let settings = expand_home("~/.claude/settings.json")?;
    if !apply {
        println!("[dry-run] would remove dir: {}", plugin_dir.display());
        println!("[dry-run] would unset `rtrt` in {}", settings.display());
        println!("Re-run with --apply to remove.");
        return Ok(());
    }
    if plugin_dir.exists() {
        std::fs::remove_dir_all(&plugin_dir)
            .with_context(|| format!("rm -rf {}", plugin_dir.display()))?;
        println!("removed {}", plugin_dir.display());
    } else {
        println!("plugin dir not present: {}", plugin_dir.display());
    }
    if settings.exists() {
        backup_if_needed(&settings)?;
        let raw = std::fs::read_to_string(&settings)
            .with_context(|| format!("read {}", settings.display()))?;
        if raw.trim().is_empty() {
            println!("{}: empty; nothing to drop", settings.display());
            return Ok(());
        }
        let mut root: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("{}: not valid JSON", settings.display()))?;
        if let Some(arr) = root.get_mut("plugins").and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|v| v.as_str() != Some("rtrt"));
            if arr.len() != before {
                let rendered = serde_json::to_string_pretty(&root)?;
                std::fs::write(&settings, rendered)
                    .with_context(|| format!("write {}", settings.display()))?;
                println!("dropped `rtrt` from {}", settings.display());
            } else {
                println!("{}: `rtrt` was not listed", settings.display());
            }
        }
    }
    Ok(())
}

/// Reverse of `apply_json` / `apply_codex_toml`. Removes the `rtrt` MCP
/// entry from the agent's config file.
pub fn uninstall_agent(agent: AgentKind, apply: bool) -> Result<()> {
    match agent {
        AgentKind::Aider => {
            println!("aider has no MCP config — nothing to remove.");
            Ok(())
        }
        AgentKind::Claude => drop_json_entry("~/.claude.json", apply),
        AgentKind::Cursor => drop_json_entry("~/.cursor/mcp.json", apply),
        AgentKind::Windsurf => drop_json_entry("~/.windsurf/mcp_config.json", apply),
        AgentKind::Codex => drop_codex_toml(apply),
    }
}

fn drop_json_entry(rel: &str, apply: bool) -> Result<()> {
    let path = expand_home(rel)?;
    if !path.exists() {
        println!("{}: not present", path.display());
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would unset mcpServers.rtrt in {}",
            path.display()
        );
        return Ok(());
    }
    backup_if_needed(&path)?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{}: not valid JSON", path.display()))?;
    if let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        if servers.remove("rtrt").is_some() {
            let rendered = serde_json::to_string_pretty(&root)?;
            std::fs::write(&path, rendered).with_context(|| format!("write {}", path.display()))?;
            println!("dropped mcpServers.rtrt from {}", path.display());
            return Ok(());
        }
    }
    println!("{}: mcpServers.rtrt not present", path.display());
    Ok(())
}

fn drop_codex_toml(apply: bool) -> Result<()> {
    let path = expand_home("~/.codex/config.toml")?;
    if !path.exists() {
        println!("{}: not present", path.display());
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would unset [mcp_servers.rtrt] in {}",
            path.display()
        );
        return Ok(());
    }
    backup_if_needed(&path)?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<&str> = raw.lines().collect();
    let mut out = String::with_capacity(raw.len());
    let mut skipping = false;
    let mut removed = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "[mcp_servers.rtrt]" {
            skipping = true;
            removed = true;
            continue;
        }
        if skipping {
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                skipping = false;
            } else {
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if removed {
        std::fs::write(&path, out).with_context(|| format!("write {}", path.display()))?;
        println!("dropped [mcp_servers.rtrt] from {}", path.display());
    } else {
        println!("{}: [mcp_servers.rtrt] not present", path.display());
    }
    Ok(())
}

fn backup_if_needed(path: &Path) -> Result<()> {
    let bak = path.with_extension({
        let mut e = path
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !e.is_empty() {
            e.push('.');
        }
        e.push_str("bak");
        e
    });
    if !bak.exists() {
        std::fs::copy(path, &bak).with_context(|| format!("backup {}", bak.display()))?;
    }
    Ok(())
}
