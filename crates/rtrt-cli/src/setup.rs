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

/// Events that Claude Code's hook engine recognises today. Each one becomes
/// a `~/.claude/settings.json` entry calling `rtrt hook capture <kind>`.
/// The `kind` slug stays kebab-case so it surfaces nicely in
/// `memory_timeline` / `memory_smart_search`.
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("PostToolUseFailure", "post-tool-use-failure"),
    ("PostToolBatch", "post-tool-batch"),
    ("PreCompact", "pre-compact"),
    ("PostCompact", "post-compact"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("UserPromptExpansion", "user-prompt-expansion"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("StopFailure", "stop-failure"),
    ("SubagentStart", "subagent-start"),
    ("SubagentStop", "subagent-stop"),
    ("SessionStart", "session-start"),
    ("SessionEnd", "session-end"),
];

const CLAUDE_PLUGIN_REL: &str = "~/.claude/plugins/cache/rtrt";

struct SkillSpec {
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

struct AgentSpec {
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

const CLAUDE_SKILLS: &[SkillSpec] = &[
    SkillSpec {
        name: "output-commit",
        description: "rtrt Output Optimizer: generate terse Conventional Commits messages for staged diffs.",
        body: r#"---
name: output-commit
description: rtrt Output Optimizer: generate terse Conventional Commits messages for staged diffs.
---

Use rtrt Output Optimizer style. Generate a terse Conventional Commits message for the staged diff. Subject must be <=50 chars. Add a body only when the why is non-obvious. Reply in the user's language unless commit syntax or repository convention requires otherwise. No praise, no filler, no AI attribution.
"#,
    },
    SkillSpec {
        name: "output-review",
        description: "rtrt Output Optimizer: code review findings as one line per finding.",
        body: r#"---
name: output-review
description: rtrt Output Optimizer: code review findings as one line per finding.
---

Use rtrt Output Optimizer style. Review code with one line per finding: location -> problem -> fix. Lead with defects, regressions, security issues, and missing tests. Reply in the user's language. No praise, no filler, no summary unless asked.
"#,
    },
    SkillSpec {
        name: "output-compress-file",
        description: "rtrt Output Optimizer: compress a notes or memory file in place with backup.",
        body: r#"---
name: output-compress-file
description: rtrt Output Optimizer: compress a notes or memory file in place with backup.
---

Use rtrt Output Optimizer style. Compress the target notes or memory file in place by running `rtrt compress --file <p> --in-place --backup`. Preserve technical facts, paths, commands, identifiers, numbers, and quoted errors. Reply in the user's language and report the file path plus backup path only.
"#,
    },
    SkillSpec {
        name: "output-stats",
        description: "rtrt Output Optimizer: show saved-character and estimated-token stats.",
        body: r#"---
name: output-stats
description: rtrt Output Optimizer: show saved-character and estimated-token stats.
---

Use rtrt Output Optimizer style. Show savings by running `rtrt stats`. Report real sources only; if token-log or memory data is unavailable, say unavailable. Reply in the user's language. Keep the summary compact.
"#,
    },
    SkillSpec {
        name: "output-help",
        description: "rtrt Output Optimizer: quick reference for /output levels.",
        body: r#"---
name: output-help
description: rtrt Output Optimizer: quick reference for /output levels.
---

Use rtrt Output Optimizer style. Provide a quick reference for `/output lite`, `/output full`, `/output ultra`, and `/output off`. Explain what each level does in the user's language. Keep it short and do not add unrelated setup text.
"#,
    },
    SkillSpec {
        name: "output-crew",
        description: "rtrt Output Optimizer: decision guide for compact code-location, edit, and review delegation.",
        body: r#"---
name: output-crew
description: rtrt Output Optimizer: decision guide for compact code-location, edit, and review delegation.
---

Use this decision guide when the main thread can save context by delegating a narrow task:
- Locate code / map a dir / find callers -> output-investigator
- Bounded 1–2 file edit -> output-builder
- Review a diff, branch, or file -> output-reviewer

All three agents reply terse and in the user's language to save context window. Keep delegation scoped to the listed task shapes.
"#,
    },
];

const CLAUDE_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "output-investigator",
        description: "Read-only code locator. Returns a compact file:line table for 'where is X / what calls Y / map this dir'. No fixes.",
        body: r#"---
name: output-investigator
description: Read-only code locator. Returns a compact file:line table for 'where is X / what calls Y / map this dir'. No fixes.
tools: [read_file, search_files, list_directory]
---

Terse, technically exact. Reply in the user's language. Return only a Markdown table of file:line matches, one row per hit. No explanations, no fixes, no praise. Refuse requests that ask for edits.
"#,
    },
    AgentSpec {
        name: "output-builder",
        description: "Surgical 1–2 file edit (typo fix, single-function rewrite, mechanical rename). Refuses 3+ file scope.",
        body: r#"---
name: output-builder
description: Surgical 1–2 file edit (typo fix, single-function rewrite, mechanical rename). Refuses 3+ file scope.
tools: [read_file, edit_file, search_files]
---

Terse, technically exact. Reply in the user's language. Accept only tasks touching ≤2 files. For any larger scope say 'Scope too wide — use a full agent.' Return a terse unified diff receipt (file path, lines changed, what changed). No prose.
"#,
    },
    AgentSpec {
        name: "output-reviewer",
        description: "Diff/branch/file reviewer. One finding per line: path:line: <severity>: <problem>. <fix>.",
        body: r#"---
name: output-reviewer
description: Diff/branch/file reviewer. One finding per line: path:line: <severity>: <problem>. <fix>.
tools: [read_file, search_files, list_directory]
---

Terse, technically exact. Reply in the user's language. Output format is strictly: path:line: <severity>: <problem>. <fix>. Severity values: error | warn | note. No praise. No scope creep. No summaries. Stop when findings are exhausted.
"#,
    },
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

/// Merges twelve rtrt hook entries into `~/.claude/settings.json`. Each
/// entry shells out to `rtrt hook capture <kind>` so the binary itself
/// owns the redact / dedup / save pipeline; no auxiliary shell scripts
/// are required on disk. This replaces the earlier "drop files into
/// `~/.claude/plugins/cache/rtrt/`" approach — Claude Code's plugin
/// loader expects a marketplace layout that an out-of-band copy can't
/// satisfy, but its `settings.json` hooks engine is well-documented and
/// stable.
fn install_claude_plugin(apply: bool) -> Result<()> {
    let settings = expand_home("~/.claude/settings.json")?;
    let plugin_dir = expand_home(CLAUDE_PLUGIN_REL)?;
    let rtrt_cmd = locate_rtrt_binary();
    if !apply {
        println!("[dry-run] target:      {}", settings.display());
        println!(
            "[dry-run] skill root:  {}",
            plugin_dir.join("skills").display()
        );
        println!(
            "[dry-run] agent root:  {}",
            plugin_dir.join("agents").display()
        );
        for skill in CLAUDE_SKILLS {
            println!(
                "[dry-run] skill file:  {}",
                skill_path(&plugin_dir, skill).display()
            );
        }
        for agent in CLAUDE_AGENTS {
            println!(
                "[dry-run] agent file:  {}",
                agent_path(&plugin_dir, agent).display()
            );
        }
        println!(
            "[dry-run] skill manifest: {}",
            plugin_dir.join("manifest.json").display()
        );
        println!("[dry-run] command:     {rtrt_cmd} hook capture <kind>");
        println!("[dry-run] hook events: {} entries", HOOK_EVENTS.len());
        println!("[dry-run] style hooks: {rtrt_cmd} hook style, {rtrt_cmd} hook style-inject");
        println!("[dry-run] statusLine:  {rtrt_cmd} statusline");
        println!("Re-run with --apply to merge the hook entries.");
        return Ok(());
    }
    write_claude_plugin_tree(&plugin_dir)?;
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
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        bail!("{}: hooks exists but is not an object", settings.display());
    }
    let hooks_obj = hooks.as_object_mut().unwrap();
    for (event, kind) in HOOK_EVENTS {
        let command = format!("{rtrt_cmd} hook capture {kind}");
        let entry = serde_json::json!({
            "matcher": "rtrt",
            "hooks": [
                {
                    "type": "command",
                    "command": command,
                    "timeout": 5
                }
            ]
        });
        let arr = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| serde_json::json!([]));
        if !arr.is_array() {
            bail!(
                "{}: hooks.{event} exists but is not an array",
                settings.display()
            );
        }
        let arr_mut = arr.as_array_mut().unwrap();
        // Drop any prior rtrt entry so re-running setup is idempotent.
        arr_mut.retain(|item| item.get("matcher").and_then(|v| v.as_str()) != Some("rtrt"));
        arr_mut.push(entry);
        // On UserPromptSubmit, update `/output <level>` state and reinforce
        // active Output Optimizer terse mode before memory recall output.
        if *event == "UserPromptSubmit" {
            arr_mut.push(serde_json::json!({
                "matcher": "rtrt",
                "hooks": [
                    {
                        "type": "command",
                        "command": format!("{rtrt_cmd} hook style"),
                        "timeout": 5
                    }
                ]
            }));
        }
        // On UserPromptSubmit, also inject relevant memory back into the
        // model's context. The capture entry above saves the prompt; this
        // one recalls the project's related history so the agent doesn't
        // have to call memory_recall by hand.
        if *event == "UserPromptSubmit" {
            arr_mut.push(serde_json::json!({
                "matcher": "rtrt",
                "hooks": [
                    {
                        "type": "command",
                        "command": format!("{rtrt_cmd} hook recall"),
                        "timeout": 5
                    }
                ]
            }));
        }
        // On SessionStart, inject the project's top memories into the model
        // context so background knowledge is available from turn 1 without
        // waiting for a UserPromptSubmit recall.
        if *event == "SessionStart" {
            arr_mut.push(serde_json::json!({
                "matcher": "rtrt",
                "hooks": [
                    {
                        "type": "command",
                        "command": format!("{rtrt_cmd} hook style-inject"),
                        "timeout": 5
                    }
                ]
            }));
            arr_mut.push(serde_json::json!({
                "matcher": "rtrt",
                "hooks": [
                    {
                        "type": "command",
                        "command": format!("{rtrt_cmd} hook session-inject"),
                        "timeout": 5
                    }
                ]
            }));
        }
        // On SessionEnd, run an LLM compression sweep over old rows. No-op
        // unless RTRT_AUTO_COMPRESS_LLM=1, so it costs nothing until the
        // user opts in — but then it runs without a dashboard daemon.
        // Longer timeout: an LLM round-trip per row.
        if *event == "SessionEnd" {
            arr_mut.push(serde_json::json!({
                "matcher": "rtrt",
                "hooks": [
                    {
                        "type": "command",
                        "command": format!("{rtrt_cmd} hook compress"),
                        "timeout": 120
                    }
                ]
            }));
        }
    }
    root.as_object_mut().unwrap().insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": format!("{rtrt_cmd} statusline")
        }),
    );
    let plugins = root
        .as_object_mut()
        .unwrap()
        .entry("plugins")
        .or_insert_with(|| serde_json::json!([]));
    if !plugins.is_array() {
        bail!("{}: plugins exists but is not an array", settings.display());
    }
    let plugins_arr = plugins.as_array_mut().unwrap();
    if !plugins_arr.iter().any(|v| v.as_str() == Some("rtrt")) {
        plugins_arr.push(serde_json::json!("rtrt"));
    }
    let rendered = serde_json::to_string_pretty(&root)?;
    std::fs::write(&settings, rendered).with_context(|| format!("write {}", settings.display()))?;
    println!(
        "merged {} hook entries (+ auto-recall on UserPromptSubmit) into {}",
        HOOK_EVENTS.len(),
        settings.display()
    );
    Ok(())
}

fn skill_path(plugin_dir: &Path, skill: &SkillSpec) -> PathBuf {
    plugin_dir.join("skills").join(skill.name).join("SKILL.md")
}

fn agent_path(plugin_dir: &Path, agent: &AgentSpec) -> PathBuf {
    plugin_dir.join("agents").join(format!("{}.md", agent.name))
}

fn write_claude_plugin_tree(plugin_dir: &Path) -> Result<()> {
    let skills_dir = plugin_dir.join("skills");
    let agents_dir = plugin_dir.join("agents");
    std::fs::create_dir_all(&skills_dir)
        .with_context(|| format!("mkdir {}", skills_dir.display()))?;
    std::fs::create_dir_all(&agents_dir)
        .with_context(|| format!("mkdir {}", agents_dir.display()))?;
    for skill in CLAUDE_SKILLS {
        let path = skill_path(plugin_dir, skill);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(&path, skill.body).with_context(|| format!("write {}", path.display()))?;
    }
    for agent in CLAUDE_AGENTS {
        let path = agent_path(plugin_dir, agent);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(&path, agent.body).with_context(|| format!("write {}", path.display()))?;
    }
    let manifest_skills: Vec<serde_json::Value> = CLAUDE_SKILLS
        .iter()
        .map(|skill| {
            serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "path": format!("skills/{}/SKILL.md", skill.name),
            })
        })
        .collect();
    let manifest_agents: Vec<serde_json::Value> = CLAUDE_AGENTS
        .iter()
        .map(|agent| {
            serde_json::json!({
                "name": agent.name,
                "description": agent.description,
                "path": format!("agents/{}.md", agent.name),
            })
        })
        .collect();
    let manifest = serde_json::json!({
        "name": "rtrt",
        "description": "rtrt Output Optimizer Claude Code plugin.",
        "version": env!("CARGO_PKG_VERSION"),
        "skills": manifest_skills,
        "agents": manifest_agents,
    });
    let manifest_path = plugin_dir.join("manifest.json");
    let rendered = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, rendered)
        .with_context(|| format!("write {}", manifest_path.display()))?;
    println!(
        "wrote {} rtrt Output Optimizer skill files under {}",
        CLAUDE_SKILLS.len(),
        skills_dir.display()
    );
    println!(
        "wrote {} rtrt Output Optimizer agent files under {}",
        CLAUDE_AGENTS.len(),
        agents_dir.display()
    );
    Ok(())
}

/// Reverse of `install_claude_plugin`. Removes every rtrt-tagged hook
/// from `~/.claude/settings.json`. Older installs also dropped the
/// plugin cache directory + a `plugins` array entry — those are cleared
/// here too so an upgrade-in-place is clean.
pub fn uninstall_claude_plugin(apply: bool) -> Result<()> {
    let settings = expand_home("~/.claude/settings.json")?;
    let legacy_plugin_dir = expand_home("~/.claude/plugins/cache/rtrt")?;
    if !apply {
        println!(
            "[dry-run] would unset rtrt hook entries in {}",
            settings.display()
        );
        if legacy_plugin_dir.exists() {
            println!(
                "[dry-run] would remove legacy dir {}",
                legacy_plugin_dir.display()
            );
        }
        return Ok(());
    }
    if legacy_plugin_dir.exists() {
        std::fs::remove_dir_all(&legacy_plugin_dir)
            .with_context(|| format!("rm -rf {}", legacy_plugin_dir.display()))?;
        println!("removed legacy {}", legacy_plugin_dir.display());
    }
    if !settings.exists() {
        println!("{}: not present; nothing to drop", settings.display());
        return Ok(());
    }
    backup_if_needed(&settings)?;
    let raw = std::fs::read_to_string(&settings)
        .with_context(|| format!("read {}", settings.display()))?;
    if raw.trim().is_empty() {
        println!("{}: empty; nothing to drop", settings.display());
        return Ok(());
    }
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{}: not valid JSON", settings.display()))?;
    let mut touched = false;
    if let Some(hooks) = root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        for entries in hooks.values_mut() {
            if let Some(arr) = entries.as_array_mut() {
                let before = arr.len();
                arr.retain(|item| item.get("matcher").and_then(|v| v.as_str()) != Some("rtrt"));
                if arr.len() != before {
                    touched = true;
                }
            }
        }
        // Drop any hook event keys that we left empty after stripping rtrt
        // — Claude Code's settings parser warns on event names that are
        // either unrecognised or carry no matchers, so an empty array is
        // worse than the missing key.
        let empty_keys: Vec<String> = hooks
            .iter()
            .filter_map(|(k, v)| v.as_array().filter(|arr| arr.is_empty()).map(|_| k.clone()))
            .collect();
        for k in empty_keys {
            hooks.remove(&k);
            touched = true;
        }
    }
    if let Some(arr) = root.get_mut("plugins").and_then(|v| v.as_array_mut()) {
        let before = arr.len();
        arr.retain(|v| v.as_str() != Some("rtrt"));
        if arr.len() != before {
            touched = true;
        }
    }
    if touched {
        let rendered = serde_json::to_string_pretty(&root)?;
        std::fs::write(&settings, rendered)
            .with_context(|| format!("write {}", settings.display()))?;
        println!("dropped rtrt hook entries from {}", settings.display());
    } else {
        println!("{}: no rtrt entries to drop", settings.display());
    }
    Ok(())
}

/// Pick the `rtrt` command to embed in the hook line. Prefers the binary
/// next to the running CLI; falls back to the bare `rtrt` symbol so
/// `PATH` lookup still works on machines without `~/.local/bin` quoting.
fn locate_rtrt_binary() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("rtrt");
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "rtrt".to_string()
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
