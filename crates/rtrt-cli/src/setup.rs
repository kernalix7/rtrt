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
use rtrt_core::OutputStyleLevel;

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

const CLAUDE_SKILLS_ROOT_REL: &str = "~/.claude/skills";
const CLAUDE_AGENTS_ROOT_REL: &str = "~/.claude/agents";
const CURSOR_RULES_REL: &str = "~/.cursor/rules/rtrt-output-optimizer.mdc";
const WINDSURF_RULES_REL: &str = "~/.codeium/windsurf/memories/global_rules.md";
const CODEX_RULES_REL: &str = "~/.codex/AGENTS.md";
const AIDER_RULES_REL: &str = "~/.aider/conventions.md";
const TERSE_BLOCK_BEGIN: &str = "# BEGIN rtrt-output-optimizer";
const TERSE_BLOCK_END: &str = "# END rtrt-output-optimizer";
const DEFAULT_AGENT_STYLE_LEVEL: OutputStyleLevel = OutputStyleLevel::Full;
const HOOK_COMMAND_TIMEOUT_SECONDS: u64 = 5;
const PROXY_REWRITE_EVENT: &str = "PreToolUse";
const PROXY_REWRITE_MATCHER: &str = "Bash";
const PROXY_REWRITE_COMMAND: &str = "rtrt hook proxy-rewrite";
const COMMAND_HOOK_TYPE: &str = "command";

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
tools: Read, Grep, Glob
model: inherit
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
tools: Read, Edit, Grep
model: sonnet
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
tools: Read, Grep, Bash
model: sonnet
---

Terse, technically exact. Reply in the user's language. Output format is strictly: path:line: <severity>: <problem>. <fix>. Severity values: error | warn | note. No praise. No scope creep. No summaries. Stop when findings are exhausted.
"#,
    },
];

pub fn style_reinforcement(level: OutputStyleLevel) -> String {
    format!(
        "OUTPUT-OPTIMIZER: stay terse (level {}). Detect the conversation language and answer terse in that same language. Keep code/commits/PRs/security normal; do not compress security warnings, irreversible-action confirmations, ambiguous multi-step sequences, or clarification requests.",
        level.as_str()
    )
}

pub fn style_session_block(level: OutputStyleLevel) -> String {
    let rules = match level {
        OutputStyleLevel::Lite => {
            "Lite: trim filler and hedging only. Drop language-appropriate filler, for example English filler phrases, Korean 군더더기, Japanese 冗長な表現, or Spanish relleno. Keep normal grammar."
        }
        OutputStyleLevel::Full => {
            "Full: also drop grammatically optional function words where the language allows. Examples, not limits: English articles a/an/the; Korean 불필요한 조사·군더더기 존댓말 축약; Japanese 冗長な助詞・敬語; Chinese 虚词. Use readable fragments when natural in the user's language."
        }
        OutputStyleLevel::Ultra => {
            "Ultra: maximally terse. Use abbreviations, -> arrows for causality, and drop conjunctions where clear. Still write in the user's language. Never omit or blur a technical fact."
        }
        OutputStyleLevel::Off => "",
    };
    format!(
        "OUTPUT-OPTIMIZER MODE ACTIVE — level: {}\n\nYou are in Output Optimizer terse mode. Detect the language of the conversation and answer terse in that same language, whether Korean, Japanese, Chinese, Spanish, German, English, or any other language. Preserve every technical fact, identifier, command, file path, number, and quoted error exactly. No preamble, no filler, no hedging, and no restatement of the user's request.\n\n{rules}\n\nExemptions: keep code, commit messages, PR text, and security content normal. Never compress security warnings, irreversible-action confirmations, ambiguous multi-step sequences, or clarification requests. If terse wording risks ambiguity, write that part normally, then resume terse mode.",
        level.as_str()
    )
}

fn terse_rules_block() -> String {
    format!(
        "{TERSE_BLOCK_BEGIN}\n{}\n{TERSE_BLOCK_END}\n",
        style_session_block(DEFAULT_AGENT_STYLE_LEVEL)
    )
}

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
    if matches!(plan.agent, AgentKind::Claude) {
        install_claude_skills_agents(plan.apply)?;
    }
    if plan.plugin {
        install_claude_plugin(plan.apply)?;
    }
    match plan.agent {
        AgentKind::Aider => {
            install_terse_rules(plan.agent, plan.apply)?;
            println!(
                "aider has no MCP config file. To use RTRT alongside aider:\n\
                 \n\
                 1. Start the MCP server in a separate shell:\n\
                 \n\
                       {binary} --memory $HOME/.rtrt/memory.sqlite\n\
                 \n\
                 2. Use RTRT's CLI from inside aider (e.g. `/run rtrt compress -l ultra < ...`).\n",
            );
            println!(
                "For aider prompt rules, start aider with `--read {AIDER_RULES_REL}` if it does not load that file automatically."
            );
            Ok(())
        }
        AgentKind::Claude => apply_json(&plan, "~/.claude.json", &binary, &memory_path),
        AgentKind::Cursor => {
            install_terse_rules(plan.agent, plan.apply)?;
            apply_json(&plan, "~/.cursor/mcp.json", &binary, &memory_path)
        }
        AgentKind::Windsurf => {
            install_terse_rules(plan.agent, plan.apply)?;
            apply_json(&plan, "~/.windsurf/mcp_config.json", &binary, &memory_path)
        }
        AgentKind::Codex => {
            install_terse_rules(plan.agent, plan.apply)?;
            apply_codex_toml(&plan, &binary, &memory_path)
        }
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
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", path.display()))?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if !servers.is_object() {
        bail!("{}: mcpServers exists but is not an object", path.display());
    }
    servers
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: mcpServers is not an object", path.display()))?
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

fn proxy_rewrite_hook_entry() -> serde_json::Value {
    serde_json::json!({
        "matcher": PROXY_REWRITE_MATCHER,
        "hooks": [
            {
                "type": COMMAND_HOOK_TYPE,
                "command": PROXY_REWRITE_COMMAND,
                "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
            }
        ]
    })
}

fn push_hook_entry_if_missing(entries: &mut Vec<serde_json::Value>, candidate: serde_json::Value) {
    if entries.iter().any(proxy_rewrite_entry_matches) {
        return;
    }
    entries.push(candidate);
}

fn proxy_rewrite_entry_matches(entry: &serde_json::Value) -> bool {
    if entry.get("matcher").and_then(|v| v.as_str()) != Some(PROXY_REWRITE_MATCHER) {
        return false;
    }
    entry
        .get("hooks")
        .and_then(|v| v.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("type").and_then(|v| v.as_str()) == Some(COMMAND_HOOK_TYPE)
                    && hook.get("command").and_then(|v| v.as_str()) == Some(PROXY_REWRITE_COMMAND)
            })
        })
}

/// Merges rtrt hook entries into `~/.claude/settings.json`. Each
/// entry shells out to `rtrt hook capture <kind>` so the binary itself
/// owns the redact / dedup / save pipeline; no auxiliary shell scripts
/// are required on disk.
fn install_claude_plugin(apply: bool) -> Result<()> {
    let settings = expand_home("~/.claude/settings.json")?;
    let rtrt_cmd = locate_rtrt_binary();
    if !apply {
        println!("[dry-run] target:      {}", settings.display());
        println!("[dry-run] command:     {rtrt_cmd} hook capture <kind>");
        println!("[dry-run] hook events: {} entries", HOOK_EVENTS.len());
        println!("[dry-run] style hooks: {rtrt_cmd} hook style, {rtrt_cmd} hook style-inject");
        println!(
            "[dry-run] Command Optimizer hook:\n{}",
            serde_json::to_string_pretty(&proxy_rewrite_hook_entry())
                .unwrap_or_else(|_| String::new())
        );
        println!("[dry-run] statusLine:  {rtrt_cmd} statusline");
        println!("Re-run with --apply to merge the hook entries.");
        return Ok(());
    }
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
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", settings.display()))?;
    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        bail!("{}: hooks exists but is not an object", settings.display());
    }
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: hooks is not an object", settings.display()))?;
    for (event, kind) in HOOK_EVENTS {
        let command = format!("{rtrt_cmd} hook capture {kind}");
        let entry = serde_json::json!({
            "matcher": "rtrt",
            "hooks": [
                {
                    "type": "command",
                    "command": command,
                    "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
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
        let arr_mut = arr.as_array_mut().ok_or_else(|| {
            anyhow::anyhow!("{}: hooks.{event} is not an array", settings.display())
        })?;
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
                        "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
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
                        "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
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
                        "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
                    }
                ]
            }));
            arr_mut.push(serde_json::json!({
                "matcher": "rtrt",
                "hooks": [
                    {
                        "type": "command",
                        "command": format!("{rtrt_cmd} hook session-inject"),
                        "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
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
    let arr = hooks_obj
        .entry(PROXY_REWRITE_EVENT.to_string())
        .or_insert_with(|| serde_json::json!([]));
    if !arr.is_array() {
        bail!(
            "{}: hooks.{PROXY_REWRITE_EVENT} exists but is not an array",
            settings.display()
        );
    }
    let arr_mut = arr.as_array_mut().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: hooks.{PROXY_REWRITE_EVENT} is not an array",
            settings.display()
        )
    })?;
    push_hook_entry_if_missing(arr_mut, proxy_rewrite_hook_entry());
    root_obj.insert(
        "statusLine".to_string(),
        serde_json::json!({
            "type": "command",
            "command": format!("{rtrt_cmd} statusline")
        }),
    );
    let rendered = serde_json::to_string_pretty(&root)?;
    std::fs::write(&settings, rendered).with_context(|| format!("write {}", settings.display()))?;
    println!(
        "merged {} hook entries (+ auto-recall on UserPromptSubmit) into {}",
        HOOK_EVENTS.len(),
        settings.display()
    );
    Ok(())
}

fn claude_skill_path(skills_root: &Path, skill: &SkillSpec) -> PathBuf {
    skills_root.join(skill.name).join("SKILL.md")
}

fn claude_agent_path(agents_root: &Path, agent: &AgentSpec) -> PathBuf {
    agents_root.join(format!("{}.md", agent.name))
}

fn install_claude_skills_agents(apply: bool) -> Result<()> {
    let skills_root = expand_home(CLAUDE_SKILLS_ROOT_REL)?;
    let agents_root = expand_home(CLAUDE_AGENTS_ROOT_REL)?;
    if !apply {
        println!("[dry-run] Claude skill root: {}", skills_root.display());
        println!("[dry-run] Claude agent root: {}", agents_root.display());
        for skill in CLAUDE_SKILLS {
            println!(
                "[dry-run] Claude skill file: {} ({})",
                claude_skill_path(&skills_root, skill).display(),
                skill.description
            );
        }
        for agent in CLAUDE_AGENTS {
            println!(
                "[dry-run] Claude agent file: {} ({})",
                claude_agent_path(&agents_root, agent).display(),
                agent.description
            );
        }
        return Ok(());
    }
    std::fs::create_dir_all(&skills_root)
        .with_context(|| format!("mkdir {}", skills_root.display()))?;
    std::fs::create_dir_all(&agents_root)
        .with_context(|| format!("mkdir {}", agents_root.display()))?;
    for skill in CLAUDE_SKILLS {
        let path = claude_skill_path(&skills_root, skill);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(&path, skill.body).with_context(|| format!("write {}", path.display()))?;
    }
    for agent in CLAUDE_AGENTS {
        let path = claude_agent_path(&agents_root, agent);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(&path, agent.body).with_context(|| format!("write {}", path.display()))?;
    }
    println!(
        "wrote {} rtrt Output Optimizer skill files under {}",
        CLAUDE_SKILLS.len(),
        skills_root.display()
    );
    println!(
        "wrote {} rtrt Output Optimizer agent files under {}",
        CLAUDE_AGENTS.len(),
        agents_root.display()
    );
    Ok(())
}

fn terse_rules_path(agent: AgentKind) -> Option<&'static str> {
    match agent {
        AgentKind::Claude => None,
        AgentKind::Cursor => Some(CURSOR_RULES_REL),
        AgentKind::Windsurf => Some(WINDSURF_RULES_REL),
        AgentKind::Codex => Some(CODEX_RULES_REL),
        AgentKind::Aider => Some(AIDER_RULES_REL),
    }
}

fn install_terse_rules(agent: AgentKind, apply: bool) -> Result<()> {
    let Some(rel_path) = terse_rules_path(agent) else {
        return Ok(());
    };
    let path = expand_home(rel_path)?;
    let block = terse_rules_block();
    if !apply {
        println!("[dry-run] terse rules target: {}", path.display());
        println!("[dry-run] terse rules block:\n{block}");
        if matches!(agent, AgentKind::Aider) {
            println!(
                "[dry-run] aider note: start aider with `--read {rel_path}` if it does not load that file automatically."
            );
        }
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
    let rendered = upsert_terse_block(&existing, &block);
    std::fs::write(&path, rendered).with_context(|| format!("write {}", path.display()))?;
    println!(
        "wrote rtrt Output Optimizer terse rules to {}",
        path.display()
    );
    if matches!(agent, AgentKind::Aider) {
        println!(
            "For aider prompt rules, start aider with `--read {rel_path}` if it does not load that file automatically."
        );
    }
    Ok(())
}

fn upsert_terse_block(existing: &str, block: &str) -> String {
    let (mut out, _) = remove_terse_block_from_text(existing);
    if !out.trim().is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.trim().is_empty() {
        out.push('\n');
    }
    out.push_str(block);
    out
}

fn remove_terse_block_from_text(existing: &str) -> (String, bool) {
    let mut out = String::with_capacity(existing.len());
    let mut skipping = false;
    let mut removed = false;
    for line in existing.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\r', '\n']).trim();
        if marker == TERSE_BLOCK_BEGIN {
            skipping = true;
            removed = true;
            continue;
        }
        if skipping {
            if marker == TERSE_BLOCK_END {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
    }
    (out, removed)
}

/// Reverse of `install_claude_plugin`. Removes every rtrt-tagged hook
/// from `~/.claude/settings.json`. Older installs also dropped a plugin
/// cache directory and a `plugins` array entry; those are cleared here too
/// so an upgrade-in-place is clean.
pub fn uninstall_claude_plugin(apply: bool) -> Result<()> {
    let settings = expand_home("~/.claude/settings.json")?;
    let legacy_plugin_dir = expand_home("~/.claude/plugins/cache/rtrt")?;
    uninstall_claude_skills_agents(apply)?;
    remove_all_terse_rules(apply)?;
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

fn uninstall_claude_skills_agents(apply: bool) -> Result<()> {
    let skills_root = expand_home(CLAUDE_SKILLS_ROOT_REL)?;
    let agents_root = expand_home(CLAUDE_AGENTS_ROOT_REL)?;
    if !apply {
        for skill in CLAUDE_SKILLS {
            println!(
                "[dry-run] would remove Claude skill dir {}",
                skills_root.join(skill.name).display()
            );
        }
        for agent in CLAUDE_AGENTS {
            println!(
                "[dry-run] would remove Claude agent file {}",
                claude_agent_path(&agents_root, agent).display()
            );
        }
        return Ok(());
    }
    for skill in CLAUDE_SKILLS {
        let path = skills_root.join(skill.name);
        if path.exists() {
            std::fs::remove_dir_all(&path).with_context(|| format!("rm -rf {}", path.display()))?;
            println!("removed Claude skill dir {}", path.display());
        }
    }
    for agent in CLAUDE_AGENTS {
        let path = claude_agent_path(&agents_root, agent);
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| format!("rm {}", path.display()))?;
            println!("removed Claude agent file {}", path.display());
        }
    }
    Ok(())
}

fn remove_terse_rules(agent: AgentKind, apply: bool) -> Result<()> {
    let Some(rel_path) = terse_rules_path(agent) else {
        return Ok(());
    };
    let path = expand_home(rel_path)?;
    if !path.exists() {
        println!("{}: not present", path.display());
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would remove rtrt Output Optimizer terse rules block from {}",
            path.display()
        );
        return Ok(());
    }
    backup_if_needed(&path)?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let (rendered, removed) = remove_terse_block_from_text(&raw);
    if removed {
        std::fs::write(&path, rendered).with_context(|| format!("write {}", path.display()))?;
        println!(
            "removed rtrt Output Optimizer terse rules block from {}",
            path.display()
        );
    } else {
        println!(
            "{}: rtrt Output Optimizer terse rules block not present",
            path.display()
        );
    }
    Ok(())
}

fn remove_all_terse_rules(apply: bool) -> Result<()> {
    for agent in [
        AgentKind::Cursor,
        AgentKind::Windsurf,
        AgentKind::Codex,
        AgentKind::Aider,
    ] {
        remove_terse_rules(agent, apply)?;
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
            remove_terse_rules(agent, apply)?;
            println!("aider has no MCP config — nothing to remove.");
            Ok(())
        }
        AgentKind::Claude => {
            uninstall_claude_skills_agents(apply)?;
            drop_json_entry("~/.claude.json", apply)
        }
        AgentKind::Cursor => {
            remove_terse_rules(agent, apply)?;
            drop_json_entry("~/.cursor/mcp.json", apply)
        }
        AgentKind::Windsurf => {
            remove_terse_rules(agent, apply)?;
            drop_json_entry("~/.windsurf/mcp_config.json", apply)
        }
        AgentKind::Codex => {
            remove_terse_rules(agent, apply)?;
            drop_codex_toml(apply)
        }
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
