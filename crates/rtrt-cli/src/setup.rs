//! `rtrt setup --agent <name>` — wire RTRT into popular coding agents.
//!
//! Supported agents:
//! - `claude`   — `~/.claude.json` (`mcpServers.rtrt`)
//! - `cursor`   — `~/.cursor/mcp.json` (`mcpServers.rtrt`)
//! - `windsurf` — `~/.windsurf/mcp_config.json` (`mcpServers.rtrt`)
//! - `codex`    — `~/.codex/config.toml` (`[mcp_servers.rtrt]`)
//! - `opencode` — targets whichever of `~/.config/opencode/opencode.json` /
//!   `opencode.jsonc` already exists (preferring `.json`; opencode itself
//!   reads `.json` first and a fresh install creates only that file, so
//!   `.jsonc` is picked up only when it's the sole file present). `mcp.rtrt`,
//!   `type: "local"`; rules to `~/.config/opencode/AGENTS.md`. Both
//!   extensions can carry JSONC content. Config changes use the bounded JSONC
//!   object mutation framework so comments, trailing commas, and unrelated
//!   settings survive.
//! - `aider`    — prints env-var hint; aider has no MCP config file.
//!
//! Default behaviour is **dry-run**: print the path + snippet so the user can
//! review. Pass `--apply` to write the merged config. A `.bak` is written
//! alongside the original on first apply.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rtrt_core::{OutputStyleLevel, config::CLAUDE_PERMISSION_PROMPT_TOOL};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AgentKind {
    Claude,
    Cursor,
    Windsurf,
    Codex,
    Opencode,
    Aider,
}

pub struct SetupPlan {
    pub agent: AgentKind,
    pub apply: bool,
    pub memory_path: Option<PathBuf>,
    pub binary: PathBuf,
    pub plugin: bool,
    pub sandbox: bool,
    pub no_sandbox: bool,
    pub machine_only: bool,
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
const OPENCODE_MCP_CONFIG_JSON_REL: &str = "~/.config/opencode/opencode.json";
const OPENCODE_MCP_CONFIG_JSONC_REL: &str = "~/.config/opencode/opencode.jsonc";
const OPENCODE_RULES_REL: &str = "~/.config/opencode/AGENTS.md";
const OPENCODE_AGENTS_ROOT_REL: &str = "~/.config/opencode/agents";
const OPENCODE_MANAGER_AGENT: &str = "rtrt-manager";
const OPENCODE_AGENT_STATE_FILE: &str = ".rtrt-managed-state.json";
const OPENCODE_AGENT_STATE_OWNER: &str = "rtrt-opencode-task-agents";
const CLAUDE_PERMISSION_PROMPT_FLAG: &str = "permission-prompt-tool";
const OPENCODE_AGENT_BEGIN: &str = "<!-- BEGIN rtrt-managed OpenCode Task agent -->";
const OPENCODE_AGENT_END: &str = "<!-- END rtrt-managed OpenCode Task agent -->";
const OPENCODE_PROVENANCE_PLUGIN_REL: &str = "~/.config/opencode/plugins/rtrt-provenance.js";
/// Relative registration emitted by older RTRT versions. OpenCode resolves it
/// against the current package/project, not the global config directory.
const OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID: &str = "./plugins/rtrt-provenance.js";
const OPENCODE_PROVENANCE_STATE_FILE: &str = ".rtrt-provenance-state.json";
const OPENCODE_PROVENANCE_STATE_OWNER: &str = "rtrt-opencode-provenance-plugin";
const OPENCODE_PROVENANCE_STATE_VERSION: u64 = 1;
const OPENCODE_TUI_CONFIG_JSON_REL: &str = "~/.config/opencode/tui.json";
const OPENCODE_TUI_CONFIG_JSONC_REL: &str = "~/.config/opencode/tui.jsonc";
const OPENCODE_TUI_ROOT_REL: &str = "~/.config/opencode/tui";
const OPENCODE_TUI_STATUSLINE_PLUGIN_ID: &str = "./tui/rtrt-statusline.tsx";
const OPENCODE_TUI_STATUSLINE_FILE: &str = "rtrt-statusline.tsx";
const OPENCODE_TUI_STATUSLINE_CORE_FILE: &str = "rtrt-statusline-core.mjs";
const OPENCODE_TUI_KEYBIND_STATE_FILE: &str = ".rtrt-history-keybind-state.json";
const OPENCODE_TUI_KEYBIND_STATE_OWNER: &str = "rtrt-opencode-history-keybinds";
const OPENCODE_TUI_STATUSLINE_SOURCE: &str =
    include_str!("../../../plugins/opencode/tui/rtrt-statusline.tsx");
const OPENCODE_TUI_STATUSLINE_CORE_SOURCE: &str =
    include_str!("../../../plugins/opencode/tui/rtrt-statusline-core.mjs");
const OPENCODE_TUI_STATUSLINE_BEGIN: &str = "// BEGIN rtrt-managed rtrt-statusline.tsx";
const OPENCODE_TUI_STATUSLINE_END: &str = "// END rtrt-managed rtrt-statusline.tsx";
const OPENCODE_TUI_STATUSLINE_CORE_BEGIN: &str = "// BEGIN rtrt-managed rtrt-statusline-core.mjs";
const OPENCODE_TUI_STATUSLINE_CORE_END: &str = "// END rtrt-managed rtrt-statusline-core.mjs";
const OPENCODE_PROVENANCE_PLUGIN: &str =
    include_str!("../../../plugins/opencode/rtrt-provenance.js");
const OPENCODE_PROVENANCE_PLUGIN_BEGIN: &str = "// BEGIN rtrt-managed provenance plugin";
const OPENCODE_PROVENANCE_PLUGIN_END: &str = "// END rtrt-managed provenance plugin";
// Exact 2,139-byte unmarked v1 payload (Git blob d7fbbeae); do not broaden this match.
const OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1: &str = r#"import path from "node:path"

const RTRT_AGENT_TOOLS = new Set([
  "rtrt_agent_call",
  "rtrt_agent_route",
  "rtrt_team_dispatch",
])

export const RtrtProvenance = async ({ project, directory, worktree }) => {
  const agents = new Map()
  const invocations = new Map()
  const parentWorktree = project.worktree || worktree || directory
  const parentProject = path.basename(parentWorktree)

  const invocationFor = (callID) => {
    if (!callID) return crypto.randomUUID()
    let invocationID = invocations.get(callID)
    if (!invocationID) {
      invocationID = crypto.randomUUID()
      invocations.set(callID, invocationID)
    }
    return invocationID
  }

  const rememberAgent = (sessionID, agent) => {
    if (sessionID && agent) agents.set(sessionID, agent)
  }

  return {
    "chat.message": async (input) => {
      rememberAgent(input.sessionID, input.agent)
    },
    "chat.params": async (input) => {
      rememberAgent(input.sessionID, input.agent)
    },
    "tool.execute.before": async (input, output) => {
      const invocationID = invocationFor(input.callID)
      if (!RTRT_AGENT_TOOLS.has(input.tool)) return

      output.args.invocation_id = invocationID
      output.args.parent_project = parentProject
      output.args.parent_session_id = input.sessionID
      output.args.parent_call_id = input.callID
      output.args.caller_agent = agents.get(input.sessionID)
      output.args.parent_cwd = directory
      output.args.parent_worktree = parentWorktree
    },
    "shell.env": async (input, output) => {
      output.env.RTRT_INVOCATION_ID = invocationFor(input.callID)
      output.env.RTRT_PARENT_PROJECT = parentProject
      output.env.RTRT_PARENT_CWD = input.cwd
      output.env.RTRT_PARENT_WORKTREE = parentWorktree
      if (input.sessionID) {
        output.env.RTRT_PARENT_SESSION_ID = input.sessionID
        const agent = agents.get(input.sessionID)
        if (agent) output.env.RTRT_PARENT_AGENT = agent
      }
      if (input.callID) output.env.RTRT_PARENT_CALL_ID = input.callID
    },
    "tool.execute.after": async (input) => {
      invocations.delete(input.callID)
    },
  }
}
"#;
// Exact marked pre-broker-work v2 payload (secure worktree/temp-dir handling, no permission
// broker); do not broaden this match.
const OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2: &str = r#"// BEGIN rtrt-managed provenance plugin
import { chmod, lstat, mkdir, readFile, realpath } from "node:fs/promises"
import path from "node:path"

const RTRT_AGENT_TOOLS = new Set([
  "rtrt_agent_call",
  "rtrt_agent_route",
  "rtrt_team_dispatch",
])

const sanitizeSessionID = (sessionID) => {
  const sanitized = String(sessionID ?? "")
    .replace(/[^A-Za-z0-9_-]/g, "_")
    .slice(0, 128)
  return sanitized || "session"
}

const isWithin = (parent, child) => {
  const relative = path.relative(parent, child)
  return (
    relative === "" ||
    (!path.isAbsolute(relative) && relative !== ".." && !relative.startsWith(`..${path.sep}`))
  )
}

const readPathFile = async (file, prefix = "") => {
  const lines = (await readFile(file, "utf8")).split(/\r?\n/)
  if (lines.at(-1) === "") lines.pop()
  if (lines.length !== 1 || !lines[0].startsWith(prefix)) return undefined

  const value = lines[0].slice(prefix.length).trim()
  return value && !value.includes("\0") ? value : undefined
}

const resolveLinkedMainWorktree = async (worktree, gitFile) => {
  const rawGitDir = await readPathFile(gitFile, "gitdir:")
  if (!rawGitDir) return undefined

  const gitDirPath = path.resolve(worktree, rawGitDir)
  const gitDirStat = await lstat(gitDirPath)
  if (gitDirStat.isSymbolicLink() || !gitDirStat.isDirectory()) return undefined
  const gitDir = await realpath(gitDirPath)

  const rawCommonDir = await readPathFile(path.join(gitDir, "commondir"))
  const rawBacklink = await readPathFile(path.join(gitDir, "gitdir"))
  if (!rawCommonDir || !rawBacklink) return undefined

  const commonDirPath = path.resolve(gitDir, rawCommonDir)
  const commonDirStat = await lstat(commonDirPath)
  if (commonDirStat.isSymbolicLink() || !commonDirStat.isDirectory()) return undefined
  const commonDir = await realpath(commonDirPath)
  if (path.basename(commonDir) !== ".git") return undefined

  const mainWorktree = path.dirname(commonDir)
  if (mainWorktree === path.parse(mainWorktree).root) return undefined

  const mainGitStat = await lstat(path.join(mainWorktree, ".git"))
  if (mainGitStat.isSymbolicLink() || !mainGitStat.isDirectory()) return undefined
  if ((await realpath(path.join(mainWorktree, ".git"))) !== commonDir) return undefined

  const worktreesDir = await realpath(path.join(commonDir, "worktrees"))
  const adminRelative = path.relative(worktreesDir, gitDir)
  if (
    !adminRelative ||
    path.isAbsolute(adminRelative) ||
    adminRelative === ".." ||
    adminRelative.startsWith(`..${path.sep}`) ||
    adminRelative.includes(path.sep)
  ) {
    return undefined
  }

  const backlink = path.resolve(gitDir, rawBacklink)
  if ((await realpath(backlink)) !== (await realpath(gitFile))) return undefined
  return mainWorktree
}

const resolveProjectWorktree = async (projectWorktree) => {
  if (!projectWorktree) return undefined

  try {
    const worktree = await realpath(path.resolve(projectWorktree))
    if (worktree === path.parse(worktree).root) return undefined

    const gitEntry = path.join(worktree, ".git")
    const gitStat = await lstat(gitEntry)
    if (gitStat.isSymbolicLink()) return undefined
    if (gitStat.isDirectory()) return worktree
    if (gitStat.isFile()) return await resolveLinkedMainWorktree(worktree, gitEntry)
  } catch {
    return undefined
  }

  return undefined
}

const ensureSessionTempDir = async (parentWorktree, sessionID) => {
  const root = await realpath(parentWorktree)
  const tempRoot = path.join(root, ".rtrt", "tmp", "opencode")
  const tempDir = path.join(tempRoot, sanitizeSessionID(sessionID))

  if (!isWithin(root, tempDir)) {
    throw new Error("OpenCode session temp directory escapes parent worktree")
  }

  // Create one level at a time so symlinks cannot redirect writes outside the repository.
  for (const directory of [
    path.join(root, ".rtrt"),
    path.join(root, ".rtrt", "tmp"),
    tempRoot,
    tempDir,
  ]) {
    try {
      await mkdir(directory, { mode: 0o700 })
    } catch (error) {
      if (error?.code !== "EEXIST") throw error
    }
    const stat = await lstat(directory)
    if (stat.isSymbolicLink() || !stat.isDirectory()) {
      throw new Error(`OpenCode session temp path is not a secure directory: ${directory}`)
    }
  }

  const realTempDir = await realpath(tempDir)
  if (!isWithin(root, realTempDir)) {
    throw new Error("OpenCode session temp directory escapes parent worktree")
  }
  await chmod(tempDir, 0o700)
  return tempDir
}

export const RtrtProvenance = async ({ project, directory }) => {
  const agents = new Map()
  const invocations = new Map()
  const parentWorktree = await resolveProjectWorktree(project?.worktree)
  const parentProject = parentWorktree ? path.basename(parentWorktree) : undefined

  const invocationFor = (callID) => {
    if (!callID) return crypto.randomUUID()
    let invocationID = invocations.get(callID)
    if (!invocationID) {
      invocationID = crypto.randomUUID()
      invocations.set(callID, invocationID)
    }
    return invocationID
  }

  const rememberAgent = (sessionID, agent) => {
    if (sessionID && agent) agents.set(sessionID, agent)
  }

  return {
    "chat.message": async (input) => {
      rememberAgent(input.sessionID, input.agent)
    },
    "chat.params": async (input) => {
      rememberAgent(input.sessionID, input.agent)
    },
    "tool.execute.before": async (input, output) => {
      const invocationID = invocationFor(input.callID)
      if (!RTRT_AGENT_TOOLS.has(input.tool)) return

      output.args.invocation_id = invocationID
      output.args.parent_project = parentProject
      output.args.parent_session_id = input.sessionID
      output.args.parent_call_id = input.callID
      output.args.caller_agent = agents.get(input.sessionID)
      output.args.parent_cwd = directory
      output.args.parent_worktree = parentWorktree
    },
    "shell.env": async (input, output) => {
      if (parentWorktree) {
        const sessionTempDir = await ensureSessionTempDir(parentWorktree, input.sessionID)
        output.env.TMPDIR = sessionTempDir
        output.env.TEMP = sessionTempDir
        output.env.TMP = sessionTempDir
      }
      output.env.RTRT_INVOCATION_ID = invocationFor(input.callID)
      output.env.RTRT_PARENT_PROJECT = parentProject
      output.env.RTRT_PARENT_CWD = input.cwd
      output.env.RTRT_PARENT_WORKTREE = parentWorktree
      if (input.sessionID) {
        output.env.RTRT_PARENT_SESSION_ID = input.sessionID
        const agent = agents.get(input.sessionID)
        if (agent) output.env.RTRT_PARENT_AGENT = agent
      }
      if (input.callID) output.env.RTRT_PARENT_CALL_ID = input.callID
    },
    "tool.execute.after": async (input) => {
      invocations.delete(input.callID)
    },
  }
}
// END rtrt-managed provenance plugin
"#;
const AIDER_RULES_REL: &str = "~/.aider/conventions.md";
const TERSE_BLOCK_BEGIN: &str = "# BEGIN rtrt-output-optimizer";
const TERSE_BLOCK_END: &str = "# END rtrt-output-optimizer";
const OPENCODE_WORKSPACE_BLOCK_BEGIN: &str = "# BEGIN rtrt-opencode-workspace";
const OPENCODE_WORKSPACE_BLOCK_END: &str = "# END rtrt-opencode-workspace";
const DEFAULT_AGENT_STYLE_LEVEL: OutputStyleLevel = OutputStyleLevel::Full;
const HOOK_COMMAND_TIMEOUT_SECONDS: u64 = 5;
const PROXY_REWRITE_EVENT: &str = "PreToolUse";
const PROXY_REWRITE_MATCHER: &str = "Bash";
const PROXY_REWRITE_COMMAND: &str = "rtrt hook proxy-rewrite";
const COMMAND_HOOK_TYPE: &str = "command";
const STATUSLINE_COMMAND_SUFFIX: &str = "statusline --rich";

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
    AgentSpec {
        name: "log-analyzer",
        description: "rtrt read-only log triage. Parses logs, stack traces, and build output into root cause plus next step.",
        body: r#"---
name: log-analyzer
description: rtrt read-only log triage. Parses logs, stack traces, and build output into root cause plus next step.
tools: Read, Grep
model: inherit
---

Read logs, stack traces, and build output. Identify the most likely root cause, cite exact file paths or line references when present, and give the smallest useful next step. Do not edit files or run commands. Reply in the user's language. Keep it compact.
"#,
    },
    AgentSpec {
        name: "tech-lead",
        description: "rtrt team orchestration lead. Breaks down cross-cutting work, delegates focused agents, and integrates results.",
        body: r#"---
name: tech-lead
description: rtrt team orchestration lead. Breaks down cross-cutting work, delegates focused agents, and integrates results.
tools: Read, Grep, Glob
model: inherit
---

Plan cross-cutting work, decide when TeamCreate delegation is useful, assign focused read/edit/review tasks, and integrate results into a concise handoff. Keep ownership and verification explicit. Do not make broad edits directly; delegate or return the smallest actionable plan. Reply in the user's language.
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

fn opencode_workspace_rules_block() -> String {
    format!(
        "{OPENCODE_WORKSPACE_BLOCK_BEGIN}\nOpenCode workspace rules:\n- Agents must use project `.rtrt/tmp` for temporary files and never `/tmp` unless no project exists.\n- Independent writes may run in parallel; overlapping writes must serialize.\n- Native Task internal worktree placement remains host-owned; this guidance does not claim enforcement over it.\n{OPENCODE_WORKSPACE_BLOCK_END}\n"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenCodeSetupSurface {
    ProvenancePlugin,
    Rules,
    TuiStatusline,
    McpConfig,
    TaskAgents,
}

fn run_opencode_setup_steps(mut run: impl FnMut(OpenCodeSetupSurface) -> Result<()>) -> Result<()> {
    for surface in [
        OpenCodeSetupSurface::ProvenancePlugin,
        OpenCodeSetupSurface::Rules,
        OpenCodeSetupSurface::TuiStatusline,
        OpenCodeSetupSurface::McpConfig,
        OpenCodeSetupSurface::TaskAgents,
    ] {
        run(surface)?;
    }
    Ok(())
}

pub fn run(plan: SetupPlan) -> Result<()> {
    if plan.plugin && !matches!(plan.agent, AgentKind::Claude) {
        bail!("--plugin is only valid with --agent claude");
    }
    if (plan.sandbox || plan.no_sandbox) && !matches!(plan.agent, AgentKind::Opencode) {
        bail!("--sandbox/--no-sandbox are only valid with --agent opencode");
    }
    if plan.machine_only
        && (!matches!(plan.agent, AgentKind::Opencode) || !plan.sandbox || plan.no_sandbox)
    {
        bail!("--machine-only requires --agent opencode --sandbox and conflicts with --no-sandbox");
    }
    if plan.machine_only {
        return setup_opencode_sandbox_machine(plan.apply);
    }
    let mut opencode_confined = false;
    if plan.sandbox {
        // Preflight must precede every setup mutation.
        let backend = crate::sandbox::preflight_usable()?;
        let boundary = crate::sandbox::discover_project()?;
        let executable = std::fs::canonicalize(std::env::current_exe()?)?;
        crate::sandbox::validate_opencode_shell_executable(&boundary, &executable)?;
        let config = resolve_opencode_config_path()?;
        println!(
            "{}OpenCode sandbox backend: {}",
            if plan.apply { "" } else { "[dry-run] " },
            backend.display()
        );
        println!(
            "{}OpenCode sandbox target: {} -> shell={}",
            if plan.apply { "" } else { "[dry-run] " },
            config.display(),
            executable.display()
        );
        if plan.apply {
            enable_opencode_sandbox(&boundary, &config, &executable, &backend)?;
        }
        opencode_confined = true;
    } else if plan.no_sandbox {
        let boundary = crate::sandbox::discover_project()?;
        // Execution-capable definitions must disappear before the shell is
        // restored. If ownership validation/removal fails, sandbox teardown
        // is never attempted.
        if matches!(plan.agent, AgentKind::Opencode) {
            let agents_root = expand_home(OPENCODE_AGENTS_ROOT_REL)?;
            let config = resolve_opencode_config_path()?;
            remove_opencode_task_agents_at(&agents_root, &config, plan.apply)?;
        }
        disable_opencode_sandbox(&boundary, plan.apply)?;
    }
    let binary = plan.binary.to_string_lossy().to_string();
    // Generated agents always start project-pinned MCP mode. Arbitrary stores
    // remain an explicit user-only admin operation and are never persisted by setup.
    if plan.memory_path.is_some() {
        bail!("setup cannot install an arbitrary memory store; use project-pinned MCP mode");
    }
    let memory_path = None;
    if matches!(plan.agent, AgentKind::Claude) {
        install_claude_skills_agents(plan.apply)?;
    }
    if plan.plugin {
        install_claude_plugin(plan.apply)?;
    } else if matches!(plan.agent, AgentKind::Claude) {
        install_claude_statusline(plan.apply)?;
    }
    match plan.agent {
        AgentKind::Aider => {
            install_terse_rules(plan.agent, plan.apply)?;
            println!(
                "aider has no MCP config file. To use RTRT alongside aider:\n\
                 \n\
                 1. Start the project-pinned MCP server from the project in a separate shell:\n\
                 \n\
                       {binary}\n\
                 \n\
                 2. Use RTRT's CLI from inside aider (e.g. `/run rtrt compress -l ultra < ...`).\n",
            );
            println!(
                "For aider prompt rules, start aider with `--read {AIDER_RULES_REL}` if it does not load that file automatically."
            );
            Ok(())
        }
        AgentKind::Claude => {
            apply_json(&plan, "~/.claude.json", &binary, &memory_path)?;
            Ok(())
        }
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
        AgentKind::Opencode => {
            run_opencode_setup_steps(|surface| match surface {
                OpenCodeSetupSurface::ProvenancePlugin => {
                    install_opencode_provenance_plugin(plan.apply)
                }
                OpenCodeSetupSurface::Rules => install_opencode_agents_rules(plan.apply),
                OpenCodeSetupSurface::TuiStatusline => install_opencode_tui_statusline(plan.apply),
                OpenCodeSetupSurface::McpConfig => {
                    apply_opencode_jsonc(&plan, &binary, &memory_path)
                }
                OpenCodeSetupSurface::TaskAgents => {
                    install_opencode_task_agents(plan.apply, opencode_confined)
                }
            })?;
            println!(
                "Direct `opencode` remains globally stateful. Launch from an external terminal with `rtrt opencode -- ...` for project-private data/state."
            );
            Ok(())
        }
    }
}

fn setup_opencode_sandbox_machine(apply: bool) -> Result<()> {
    let executable = std::fs::canonicalize(std::env::current_exe()?)
        .context("canonicalize installed rtrt executable")?;
    crate::sandbox::validate_machine_executable(&executable)?;
    let backend = crate::sandbox::preflight_usable()?;
    let config = resolve_opencode_config_path()?;
    println!(
        "{}OpenCode machine sandbox: {} -> shell={} (backend {})",
        if apply { "" } else { "[dry-run] " },
        config.display(),
        executable.display(),
        backend.display()
    );
    if apply {
        enable_opencode_machine_sandbox(&config, &executable, &backend)?;
    } else {
        println!("[dry-run] projects authorized: none");
    }
    Ok(())
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

/// opencode's local-MCP-server shape (`McpLocalConfig` in opencode's own
/// config schema): `type: "local"`, `command` as an array holding the binary
/// plus every CLI arg (no separate `args` field, unlike the other agents'
/// `mcpServers` shape), and `enabled: true` so it's live without a manual
/// toggle in the opencode TUI.
fn build_opencode_entry(binary: &str, memory_path: &Option<PathBuf>) -> serde_json::Value {
    let mut command = vec![serde_json::Value::String(binary.to_string())];
    if let Some(p) = memory_path {
        command.push(serde_json::Value::String("--memory".to_string()));
        command.push(serde_json::Value::String(p.to_string_lossy().into_owned()));
    }
    serde_json::json!({
        "type": "local",
        "command": command,
        "enabled": true,
    })
}

fn render_opencode_snippet(binary: &str, memory_path: &Option<PathBuf>) -> String {
    let entry = build_opencode_entry(binary, memory_path);
    let wrapped = serde_json::json!({ "mcp": { "rtrt": entry } });
    serde_json::to_string_pretty(&wrapped).unwrap_or_else(|_| String::new())
}

/// Resolves the config file opencode will actually load.
///
/// opencode's own loader reads `~/.config/opencode/opencode.json` — a fresh
/// `opencode` install creates only that file, and `strings $(which opencode)
/// | grep -oE "opencode\.jsonc?"` shows far more references to `.json` than
/// `.jsonc` in the binary. `.jsonc` is opencode's documented *comments*
/// variant, read only when `.json` is absent. Earlier `rtrt setup` builds
/// hardcoded `.jsonc`, so on a machine where opencode had already created its
/// own `.json` the `mcp.rtrt` entry landed in a file opencode never opens.
///
/// Picks whichever file already exists, preferring `.json`; when neither
/// exists yet, targets `.json` — matching what a fresh opencode install
/// itself creates.
pub(crate) fn resolve_opencode_config_path() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(resolve_opencode_config_path_in(&home))
}

/// Read-only validation used by `rtrt opencode`. Launcher authorization must
/// never reconcile or rewrite global OpenCode configuration.
pub(crate) fn validate_opencode_sandbox_shell(path: &Path, executable: &Path) -> Result<()> {
    reject_symlink(path, "OpenCode config")?;
    let (_, root) = read_opencode_config(path)?;
    if root.get("shell").and_then(serde_json::Value::as_str) != executable.to_str() {
        bail!(
            "{}: OpenCode shell does not exactly match the RTRT-managed executable",
            path.display()
        );
    }
    Ok(())
}

/// Home-parameterized core of `resolve_opencode_config_path`, split out so
/// tests can exercise the resolution logic against a temp directory instead
/// of the real `$HOME`.
fn resolve_opencode_config_path_in(home: &Path) -> PathBuf {
    let json = expand_in_home(home, OPENCODE_MCP_CONFIG_JSON_REL);
    if json.exists() {
        return json;
    }
    let jsonc = expand_in_home(home, OPENCODE_MCP_CONFIG_JSONC_REL);
    if jsonc.exists() {
        return jsonc;
    }
    json
}

/// Applies (or dry-run previews) the `mcp.rtrt` entry into opencode's global
/// config — see [`resolve_opencode_config_path`] for which file that is.
///
/// Both `opencode.json` and `opencode.jsonc` can carry JSONC content
/// (comments + trailing commas allowed). The config is validated into an
/// object model, then [`write_opencode_config`] applies only the object diff,
/// preserving unrelated JSONC text. OpenCode auto-loads files in its global
/// `plugins/` directory, so setup removes obsolete RTRT-owned registrations
/// rather than adding another explicit plugin entry.
fn apply_opencode_jsonc(
    plan: &SetupPlan,
    binary: &str,
    memory_path: &Option<PathBuf>,
) -> Result<()> {
    let path = resolve_opencode_config_path()?;
    apply_opencode_jsonc_at(&path, plan.apply, binary, memory_path)
}

fn enable_opencode_sandbox(
    boundary: &crate::sandbox::ProjectBoundary,
    path: &Path,
    executable: &Path,
    backend: &Path,
) -> Result<()> {
    enable_opencode_sandbox_at_registry(
        boundary,
        path,
        executable,
        backend,
        &crate::sandbox::registry_path()?,
    )
}

fn enable_opencode_sandbox_at_registry(
    boundary: &crate::sandbox::ProjectBoundary,
    path: &Path,
    executable: &Path,
    backend: &Path,
    registry_path: &Path,
) -> Result<()> {
    crate::sandbox::with_registry_lock(registry_path, || {
        enable_opencode_sandbox_at_registry_locked(
            boundary,
            path,
            executable,
            backend,
            registry_path,
        )
    })
}

fn enable_opencode_sandbox_at_registry_locked(
    boundary: &crate::sandbox::ProjectBoundary,
    path: &Path,
    executable: &Path,
    backend: &Path,
    registry_path: &Path,
) -> Result<()> {
    crate::sandbox::validate_opencode_shell_executable(boundary, executable)?;
    reject_symlink(path, "OpenCode config")?;
    let (raw, mut root) = read_opencode_config(path)?;
    let before = root.clone();
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", path.display()))?;

    let mut registry = if registry_path.exists() {
        let registry = crate::sandbox::load_registry_from(registry_path, executable)?;
        if registry.get("config_path").and_then(|value| value.as_str()) != path.to_str()
            || registry.get("owned_shell").and_then(|value| value.as_str()) != executable.to_str()
            || registry.get("backend").and_then(|value| value.as_str()) != backend.to_str()
        {
            bail!(
                "existing sandbox ownership registry does not match exact config/executable/backend"
            );
        }
        if object.get("shell").and_then(|value| value.as_str()) != executable.to_str() {
            bail!("OpenCode shell was modified outside RTRT; preserving config and registry");
        }
        registry
    } else {
        crate::sandbox::new_registry(executable, backend, path, object.get("shell").cloned())?
    };
    crate::sandbox::authorize_project(&mut registry, boundary)?;
    object.insert(
        "shell".to_string(),
        serde_json::Value::String(executable.to_string_lossy().into_owned()),
    );
    let config_changed = root != before;
    if config_changed {
        write_opencode_config(path, &raw, &before, &root)?;
    }
    if let Err(registry_error) = crate::sandbox::write_registry_at(registry_path, &registry) {
        if config_changed {
            let (current_raw, current_root) = read_opencode_config(path)?;
            if let Err(rollback_error) =
                write_opencode_config(path, &current_raw, &current_root, &before)
            {
                bail!(
                    "write sandbox registry failed ({registry_error}); config rollback also failed ({rollback_error})"
                );
            }
        }
        return Err(registry_error);
    }
    println!(
        "enabled strict OpenCode shell sandbox in {}",
        path.display()
    );
    Ok(())
}

fn enable_opencode_machine_sandbox(path: &Path, executable: &Path, backend: &Path) -> Result<()> {
    let registry_path = crate::sandbox::registry_path()?;
    enable_opencode_machine_sandbox_at(path, executable, backend, &registry_path)
}

fn enable_opencode_machine_sandbox_at(
    path: &Path,
    executable: &Path,
    backend: &Path,
    registry_path: &Path,
) -> Result<()> {
    crate::sandbox::with_registry_lock(registry_path, || {
        reject_symlink(path, "OpenCode config")?;
        if path.exists() {
            crate::sandbox::validate_machine_config(path)?;
        }
        let (raw, mut root) = read_opencode_config(path)?;
        let before = root.clone();
        let object = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", path.display()))?;
        let registry = if registry_path.exists() {
            let registry = crate::sandbox::load_registry_from(registry_path, executable)?;
            if registry
                .get("config_path")
                .and_then(serde_json::Value::as_str)
                != path.to_str()
                || registry
                    .get("owned_shell")
                    .and_then(serde_json::Value::as_str)
                    != executable.to_str()
                || registry.get("backend").and_then(serde_json::Value::as_str) != backend.to_str()
            {
                bail!(
                    "existing sandbox ownership registry belongs to another installation; run its recorded rtrt uninstall before reinstalling here"
                );
            }
            if object.get("shell").and_then(serde_json::Value::as_str) != executable.to_str() {
                bail!("OpenCode shell was modified outside RTRT; preserving config and registry");
            }
            registry
        } else {
            crate::sandbox::new_registry(executable, backend, path, object.get("shell").cloned())?
        };
        object.insert(
            "shell".to_string(),
            serde_json::Value::String(executable.to_string_lossy().into_owned()),
        );
        let config_changed = root != before;
        if config_changed {
            write_opencode_config(path, &raw, &before, &root)?;
        }
        if let Err(registry_error) = crate::sandbox::write_registry_at(registry_path, &registry) {
            if config_changed {
                let (current_raw, current_root) = read_opencode_config(path)?;
                if let Err(rollback_error) =
                    write_opencode_config(path, &current_raw, &current_root, &before)
                {
                    bail!(
                        "write sandbox registry failed ({registry_error}); config rollback also failed ({rollback_error})"
                    );
                }
            }
            return Err(registry_error);
        }
        println!(
            "validated strict OpenCode machine shell in {}",
            path.display()
        );
        Ok(())
    })
}

fn disable_opencode_sandbox(boundary: &crate::sandbox::ProjectBoundary, apply: bool) -> Result<()> {
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    disable_opencode_sandbox_at_registry(
        boundary,
        apply,
        &crate::sandbox::registry_path()?,
        &executable,
    )
}

fn disable_opencode_sandbox_at_registry(
    boundary: &crate::sandbox::ProjectBoundary,
    apply: bool,
    registry_path: &Path,
    executable: &Path,
) -> Result<()> {
    if !registry_path.exists() {
        println!("OpenCode sandbox: no setup-managed global ownership registry");
        return Ok(());
    }
    crate::sandbox::with_registry_lock(registry_path, || {
        disable_opencode_sandbox_at_registry_locked(boundary, apply, registry_path, executable)
    })
}

fn disable_opencode_sandbox_at_registry_locked(
    boundary: &crate::sandbox::ProjectBoundary,
    apply: bool,
    registry_path: &Path,
    executable: &Path,
) -> Result<()> {
    let mut state = crate::sandbox::load_registry_from(registry_path, executable)?;
    let path = PathBuf::from(
        state
            .get("config_path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow::anyhow!("sandbox registry has no config_path"))?,
    );
    let owned = state
        .get("owned_shell")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("sandbox registry has no owned_shell"))?;
    let (raw, mut root) = read_opencode_config(&path)?;
    if root.get("shell").and_then(|value| value.as_str()) != Some(owned) {
        bail!(
            "{}: OpenCode shell is absent or modified; preserving config and ownership registry",
            path.display()
        );
    }
    // Validate membership before reporting or mutating anything.
    crate::sandbox::project_tool_paths(&state, boundary)?;
    let remaining_empty = {
        let projects = state["projects"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("sandbox registry projects is not an object"))?;
        projects.len() == 1
    };
    println!(
        "{}{} in {}",
        if apply { "" } else { "[dry-run] would " },
        if remaining_empty {
            "restore prior OpenCode shell"
        } else {
            "remove only this project from OpenCode sandbox registry"
        },
        path.display()
    );
    if !apply {
        return Ok(());
    }
    let last = crate::sandbox::deauthorize_project(&mut state, boundary)?;
    if last {
        let before = root.clone();
        let object = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{}: root is not an object", path.display()))?;
        match state.get("prior_shell").filter(|value| !value.is_null()) {
            Some(prior) => {
                object.insert("shell".to_string(), prior.clone());
            }
            None => {
                object.remove("shell");
            }
        }
        write_opencode_config(&path, &raw, &before, &root)?;
        if let Err(remove_error) = crate::sandbox::remove_registry_at(registry_path) {
            let (current_raw, current_root) = read_opencode_config(&path)?;
            if let Err(rollback_error) =
                write_opencode_config(&path, &current_raw, &current_root, &before)
            {
                bail!(
                    "remove sandbox registry failed ({remove_error}); config rollback also failed ({rollback_error})"
                );
            }
            return Err(remove_error);
        }
    } else {
        crate::sandbox::write_registry_at(registry_path, &state)?;
    }
    Ok(())
}

fn disable_opencode_sandbox_global(apply: bool) -> Result<()> {
    let registry_path = crate::sandbox::registry_path()?;
    if !registry_path.exists() {
        println!("OpenCode sandbox: no setup-managed global ownership registry");
        return Ok(());
    }
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    disable_opencode_sandbox_global_at(apply, &registry_path, &executable)
}

fn disable_opencode_sandbox_global_at(
    apply: bool,
    registry_path: &Path,
    executable: &Path,
) -> Result<()> {
    crate::sandbox::with_registry_lock(registry_path, || {
        let state = crate::sandbox::load_registry_from(registry_path, executable)?;
        let path = PathBuf::from(
            state
                .get("config_path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("sandbox registry has no config_path"))?,
        );
        let owned = state
            .get("owned_shell")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("sandbox registry has no owned_shell"))?;
        let (raw, mut root) = read_opencode_config(&path)?;
        if root.get("shell").and_then(serde_json::Value::as_str) != Some(owned) {
            bail!(
                "{}: OpenCode shell is absent or modified; preserving config and ownership registry",
                path.display()
            );
        }
        println!(
            "{}restore recorded prior OpenCode shell in {} and remove machine registry",
            if apply { "" } else { "[dry-run] would " },
            path.display()
        );
        if !apply {
            return Ok(());
        }
        let before = root.clone();
        let object = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{}: root is not an object", path.display()))?;
        match state.get("prior_shell").filter(|value| !value.is_null()) {
            Some(prior) => {
                object.insert("shell".to_string(), prior.clone());
            }
            None => {
                object.remove("shell");
            }
        }
        write_opencode_config(&path, &raw, &before, &root)?;
        if let Err(remove_error) = crate::sandbox::remove_registry_at(registry_path) {
            let (current_raw, current_root) = read_opencode_config(&path)?;
            if let Err(rollback_error) =
                write_opencode_config(&path, &current_raw, &current_root, &before)
            {
                bail!(
                    "remove sandbox registry failed ({remove_error}); config rollback also failed ({rollback_error})"
                );
            }
            return Err(remove_error);
        }
        Ok(())
    })
}

/// Path-parameterized core of `apply_opencode_jsonc`, split out so tests can
/// exercise the real read/backup/write flow against a temp file instead of
/// the real opencode config path.
fn apply_opencode_jsonc_at(
    path: &Path,
    apply: bool,
    binary: &str,
    memory_path: &Option<PathBuf>,
) -> Result<()> {
    let snippet = render_opencode_snippet(binary, memory_path);
    let plugin_url = opencode_provenance_plugin_url(path)?;
    if !apply {
        println!("[dry-run] target: {}", path.display());
        println!("[dry-run] snippet:\n{snippet}");
        println!("{}", opencode_provenance_dry_run_status(&plugin_url));
        println!("\nRe-run with --apply to merge into the file.");
        return Ok(());
    }
    reject_symlink(path, "OpenCode config")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let entry = build_opencode_entry(binary, memory_path);
    if !path.exists() {
        let root = serde_json::json!({
            "mcp": { "rtrt": entry },
        });
        let rendered = serde_json::to_string_pretty(&root)?;
        write_private_file_atomic_same_dir(path, rendered.as_bytes())?;
        println!(
            "wrote {} with mcp.rtrt; provenance plugin auto-loads from the OpenCode plugins directory",
            path.display(),
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut root = parse_json_or_jsonc(&raw, path)?;
    let before = root.clone();
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", path.display()))?;
    let mcp = obj.entry("mcp").or_insert_with(|| serde_json::json!({}));
    if !mcp.is_object() {
        bail!("{}: mcp exists but is not an object", path.display());
    }
    mcp.as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: mcp is not an object", path.display()))?
        .insert("rtrt".to_string(), entry);
    drop_opencode_provenance_registration(&mut root, path, &plugin_url)?;
    if root != before {
        write_opencode_config(path, &raw, &before, &root)?;
    }
    println!(
        "merged mcp.rtrt and removed legacy RTRT plugin registrations from {}",
        path.display(),
    );
    Ok(())
}

fn normalized_absolute_path(path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        bail!(
            "OpenCode provenance plugin path must be absolute: {}",
            path.display()
        );
    }
    let mut normalized = PathBuf::new();
    let mut normal_depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normal_depth == 0 {
                    bail!(
                        "OpenCode provenance plugin path escapes its absolute root: {}",
                        path.display()
                    );
                }
                normalized.pop();
                normal_depth -= 1;
            }
            Component::Normal(part) => {
                normalized.push(part);
                normal_depth += 1;
            }
        }
    }
    if !normalized.is_absolute() {
        bail!(
            "OpenCode provenance plugin path is not representable as normalized absolute path: {}",
            path.display()
        );
    }
    Ok(normalized)
}

fn percent_encode_file_url_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[cfg(unix)]
fn absolute_path_to_file_url(path: &Path) -> Result<String> {
    use std::os::unix::ffi::OsStrExt;

    let path = normalized_absolute_path(path)?;
    Ok(format!(
        "file://{}",
        percent_encode_file_url_bytes(path.as_os_str().as_bytes())
    ))
}

#[cfg(windows)]
fn absolute_path_to_file_url(path: &Path) -> Result<String> {
    let path = normalized_absolute_path(path)?;
    let text = path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: Windows plugin path contains unpaired Unicode and cannot be represented as a file URL",
            path.display()
        )
    })?;
    if text.starts_with(r"\\") {
        bail!(
            "{}: Windows UNC/device plugin paths are not supported for OpenCode file URL registration",
            path.display()
        );
    }
    let bytes = text.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        bail!(
            "{}: Windows plugin path is not an absolute drive path representable as a file URL",
            path.display()
        );
    }
    let slash_path = text.replace('\\', "/");
    Ok(format!(
        "file:///{}",
        percent_encode_file_url_bytes(slash_path.as_bytes())
    ))
}

fn opencode_provenance_plugin_url(config_path: &Path) -> Result<String> {
    let parent = config_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "{}: OpenCode config has no parent for plugin registration",
            config_path.display()
        )
    })?;
    absolute_path_to_file_url(&parent.join("plugins/rtrt-provenance.js"))
}

fn opencode_provenance_dry_run_status(plugin_url: &str) -> String {
    format!(
        "[dry-run] provenance plugin auto-loads; would remove legacy registrations {plugin_url} and {OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID}"
    )
}

fn drop_opencode_provenance_registration(
    root: &mut serde_json::Value,
    path: &Path,
    plugin_url: &str,
) -> Result<bool> {
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", path.display()))?;
    let Some(plugins) = object.get_mut("plugin") else {
        return Ok(false);
    };
    let Some(plugins) = plugins.as_array_mut() else {
        // This field is unrelated to RTRT when it is not the legacy array shape.
        return Ok(false);
    };
    let before = plugins.len();
    plugins.retain(|plugin| {
        !matches!(plugin.as_str(), Some(id) if id == OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID || id == plugin_url)
    });
    Ok(plugins.len() != before)
}

/// Byte offset of the `}` matching the `{` at `open_idx`, skipping over
/// string literals (respecting `\"` escapes) and `//` / `/* */` comments so
/// brace characters inside either don't throw off the depth count. Still a
/// best-effort scanner, not a real JSONC parser — used only by the textual
/// JSONC fallback.
fn find_matching_brace(text: &str, open_idx: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open_idx) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut i = open_idx;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Reverse of `apply_opencode_jsonc`. Strict JSON round-trips through the value
/// model; JSONC uses the bounded object editor so comments and foreign entries
/// remain byte-for-byte intact.
/// Path-parameterized core of `drop_opencode_jsonc`; see
/// `apply_opencode_jsonc_at` for why the split exists (temp-file testing).
fn drop_opencode_jsonc_at(path: &Path, apply: bool) -> Result<()> {
    let plugin_url = opencode_provenance_plugin_url(path)?;
    let Some(metadata) = path_metadata(path)? else {
        println!("{}: not present", path.display());
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{}: OpenCode config is not a real file", path.display());
    }
    if !apply {
        println!(
            "[dry-run] would unset mcp.rtrt and remove provenance plugin registrations {plugin_url} and {OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID} in {}",
            path.display()
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut root = parse_json_or_jsonc(&raw, path)?;
    let before = root.clone();
    let mut removed = false;
    if let Some(mcp) = root.get_mut("mcp").and_then(|value| value.as_object_mut())
        && mcp.remove("rtrt").is_some()
    {
        removed = true;
    }
    removed |= drop_opencode_provenance_registration(&mut root, path, &plugin_url)?;
    if !removed {
        println!(
            "{}: managed rtrt OpenCode settings not present",
            path.display()
        );
        return Ok(());
    }
    write_opencode_config(path, &raw, &before, &root)?;
    println!("dropped rtrt OpenCode settings from {}", path.display());
    Ok(())
}

fn resolve_opencode_tui_config_path() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(resolve_opencode_tui_config_path_in(&home))
}

fn resolve_opencode_tui_config_path_in(home: &Path) -> PathBuf {
    let json = expand_in_home(home, OPENCODE_TUI_CONFIG_JSON_REL);
    if json.exists() {
        return json;
    }
    let jsonc = expand_in_home(home, OPENCODE_TUI_CONFIG_JSONC_REL);
    if jsonc.exists() {
        return jsonc;
    }
    json
}

fn managed_tui_source(source: &str, begin: &str, end: &str) -> String {
    format!("{begin}\n{}\n{end}\n", source.trim_end())
}

fn managed_tui_statusline_source() -> String {
    managed_tui_source(
        OPENCODE_TUI_STATUSLINE_SOURCE,
        OPENCODE_TUI_STATUSLINE_BEGIN,
        OPENCODE_TUI_STATUSLINE_END,
    )
}

fn managed_tui_statusline_core_source() -> String {
    managed_tui_source(
        OPENCODE_TUI_STATUSLINE_CORE_SOURCE,
        OPENCODE_TUI_STATUSLINE_CORE_BEGIN,
        OPENCODE_TUI_STATUSLINE_CORE_END,
    )
}

fn exact_marker_line_range(raw: &str, marker: &str) -> Option<std::ops::Range<usize>> {
    let mut offset = 0usize;
    let mut found = None;
    for line in raw.split_inclusive('\n') {
        let text = line.strip_suffix('\n').unwrap_or(line);
        let text = text.strip_suffix('\r').unwrap_or(text);
        if text == marker {
            if found.is_some() {
                return None;
            }
            found = Some(offset..offset + line.len());
        }
        offset += line.len();
    }
    found
}

fn exact_markers_own_file(raw: &str, begin: &str, end: &str) -> bool {
    let Some(begin_range) = exact_marker_line_range(raw, begin) else {
        return false;
    };
    let Some(end_range) = exact_marker_line_range(raw, end) else {
        return false;
    };
    begin_range.start < end_range.start
        && raw[..begin_range.start].trim().is_empty()
        && raw[end_range.end..].trim().is_empty()
}

fn ensure_managed_tui_file_installable(
    path: &Path,
    current: &str,
    begin: &str,
    end: &str,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if raw == current || exact_markers_own_file(&raw, begin, end) {
        return Ok(());
    }
    bail!(
        "{}: refusing to overwrite unrecognized pre-existing OpenCode TUI file",
        path.display()
    )
}

fn install_managed_tui_file_at(
    path: &Path,
    current: &str,
    begin: &str,
    end: &str,
    apply: bool,
) -> Result<()> {
    if !apply {
        println!("[dry-run] OpenCode TUI managed file: {}", path.display());
        return Ok(());
    }
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(path, current).with_context(|| format!("write {}", path.display()))?;
        println!("wrote managed OpenCode TUI file {}", path.display());
        return Ok(());
    }

    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if raw == current {
        println!(
            "{}: managed OpenCode TUI file already current",
            path.display()
        );
        return Ok(());
    }
    if !exact_markers_own_file(&raw, begin, end) {
        bail!(
            "{}: refusing to overwrite unrecognized pre-existing OpenCode TUI file",
            path.display()
        );
    }
    let backup = backup_path(path);
    std::fs::copy(path, &backup).with_context(|| format!("backup {}", backup.display()))?;
    std::fs::write(path, current).with_context(|| format!("write {}", path.display()))?;
    println!("upgraded managed OpenCode TUI file {}", path.display());
    Ok(())
}

fn remove_managed_tui_file_at(
    path: &Path,
    current: &str,
    begin: &str,
    end: &str,
    apply: bool,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would remove managed OpenCode TUI file {}",
            path.display()
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if raw != current && !exact_markers_own_file(&raw, begin, end) {
        println!(
            "{}: content is modified or no longer marked as rtrt-managed; preserving it",
            path.display()
        );
        return Ok(());
    }

    std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    let backup = backup_path(path);
    if backup.exists() {
        let backup_raw = std::fs::read_to_string(&backup)
            .with_context(|| format!("read {}", backup.display()))?;
        if backup_raw == current || exact_markers_own_file(&backup_raw, begin, end) {
            std::fs::remove_file(&backup)
                .with_context(|| format!("remove {}", backup.display()))?;
        }
    }
    println!("removed managed OpenCode TUI file {}", path.display());
    Ok(())
}

fn jsonc_to_json(raw: &str) -> Result<String> {
    let bytes = raw.as_bytes();
    let mut without_comments = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            without_comments.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            without_comments.push(byte);
            i += 1;
        } else if byte == b'/' && bytes.get(i + 1) == Some(&b'/') {
            without_comments.extend_from_slice(b"  ");
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                without_comments.push(b' ');
                i += 1;
            }
        } else if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            without_comments.extend_from_slice(b"  ");
            i += 2;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    without_comments.extend_from_slice(b"  ");
                    i += 2;
                    closed = true;
                    break;
                }
                without_comments.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if !closed {
                bail!("unterminated JSONC block comment");
            }
        } else {
            without_comments.push(byte);
            i += 1;
        }
    }

    let mut without_trailing_commas = Vec::with_capacity(without_comments.len());
    i = 0;
    in_string = false;
    escaped = false;
    while i < without_comments.len() {
        let byte = without_comments[i];
        if in_string {
            without_trailing_commas.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
        }
        if byte == b',' {
            let mut next = i + 1;
            while next < without_comments.len() && without_comments[next].is_ascii_whitespace() {
                next += 1;
            }
            if matches!(without_comments.get(next), Some(b'}' | b']')) {
                i += 1;
                continue;
            }
        }
        without_trailing_commas.push(byte);
        i += 1;
    }
    String::from_utf8(without_trailing_commas)
        .map_err(|error| anyhow::anyhow!("JSONC conversion produced invalid UTF-8: {error}"))
}

fn parse_json_or_jsonc(raw: &str, path: &Path) -> Result<serde_json::Value> {
    match serde_json::from_str(raw) {
        Ok(root) => Ok(root),
        Err(strict_error) => {
            let normalized =
                jsonc_to_json(raw).with_context(|| format!("{}: invalid JSONC", path.display()))?;
            serde_json::from_str(&normalized).with_context(|| {
                format!(
                    "{}: invalid JSON/JSONC (strict JSON error: {strict_error})",
                    path.display()
                )
            })
        }
    }
}

fn opencode_tui_plugin_spec_id(spec: &serde_json::Value) -> Option<&str> {
    spec.as_str().or_else(|| {
        spec.as_array()
            .and_then(|tuple| tuple.first())
            .and_then(serde_json::Value::as_str)
    })
}

fn opencode_tui_statusline_tuple(
    binary: &Path,
    existing: Option<&serde_json::Value>,
) -> Result<serde_json::Value> {
    if !binary.is_absolute() {
        bail!(
            "OpenCode TUI statusline binary path must be absolute: {}",
            binary.display()
        );
    }
    let mut options = existing
        .and_then(serde_json::Value::as_array)
        .and_then(|tuple| tuple.get(1))
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    options.insert(
        "bin".to_string(),
        serde_json::Value::String(binary.to_string_lossy().into_owned()),
    );
    Ok(serde_json::json!([
        OPENCODE_TUI_STATUSLINE_PLUGIN_ID,
        options
    ]))
}

fn upsert_opencode_tui_statusline_plugin(
    root: &mut serde_json::Value,
    binary: &Path,
) -> Result<bool> {
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode TUI config root is not a JSON object"))?;
    let plugins = object
        .entry("plugin")
        .or_insert_with(|| serde_json::json!([]));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode TUI config plugin is not an array"))?;
    let before = plugins.clone();
    let mut replaced = false;
    let mut merged = Vec::with_capacity(plugins.len() + 1);
    for spec in &before {
        if opencode_tui_plugin_spec_id(spec) == Some(OPENCODE_TUI_STATUSLINE_PLUGIN_ID) {
            if !replaced {
                merged.push(opencode_tui_statusline_tuple(binary, Some(spec))?);
                replaced = true;
            }
        } else {
            merged.push(spec.clone());
        }
    }
    if !replaced {
        merged.push(opencode_tui_statusline_tuple(binary, None)?);
    }
    let changed = merged != before;
    *plugins = merged;
    Ok(changed)
}

fn drop_opencode_tui_statusline_plugin(root: &mut serde_json::Value) -> Result<bool> {
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode TUI config root is not a JSON object"))?;
    let Some(plugins) = object.get_mut("plugin") else {
        return Ok(false);
    };
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode TUI config plugin is not an array"))?;
    let before = plugins.len();
    plugins.retain(|spec| {
        opencode_tui_plugin_spec_id(spec) != Some(OPENCODE_TUI_STATUSLINE_PLUGIN_ID)
    });
    Ok(plugins.len() != before)
}

fn tui_keybind_state_path(config: &Path) -> Result<PathBuf> {
    Ok(config
        .parent()
        .context("OpenCode TUI config has no parent")?
        .join(OPENCODE_TUI_KEYBIND_STATE_FILE))
}

fn prior_keybind(
    keybinds: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> serde_json::Value {
    match keybinds.get(key) {
        Some(value) => serde_json::json!({"present": true, "value": value}),
        None => serde_json::json!({"present": false}),
    }
}

fn install_history_keybinds(
    root: &mut serde_json::Value,
    config: &Path,
) -> Result<Option<(PathBuf, serde_json::Value)>> {
    let state_path = tui_keybind_state_path(config)?;
    let object = root
        .as_object_mut()
        .context("OpenCode TUI config root is not a JSON object")?;
    let keybinds = object
        .entry("keybinds")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("OpenCode TUI config keybinds is not an object")?;
    let state = if state_path.exists() {
        reject_symlink(&state_path, "OpenCode TUI keybind ownership state")?;
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path)?)?;
        anyhow::ensure!(
            state["owner"] == OPENCODE_TUI_KEYBIND_STATE_OWNER,
            "{}: foreign OpenCode keybind ownership state",
            state_path.display()
        );
        for key in ["history_previous", "history_next"] {
            anyhow::ensure!(
                keybinds.get(key).and_then(serde_json::Value::as_str) == Some("none"),
                "OpenCode {key} was modified after RTRT setup; preserving config and ownership state"
            );
        }
        None
    } else {
        Some((
            state_path,
            serde_json::json!({
                "owner": OPENCODE_TUI_KEYBIND_STATE_OWNER,
                "history_previous": prior_keybind(keybinds, "history_previous"),
                "history_next": prior_keybind(keybinds, "history_next"),
            }),
        ))
    };
    keybinds.insert("history_previous".into(), serde_json::json!("none"));
    keybinds.insert("history_next".into(), serde_json::json!("none"));
    Ok(state)
}

fn restore_history_keybinds(
    root: &mut serde_json::Value,
    config: &Path,
) -> Result<Option<PathBuf>> {
    let state_path = tui_keybind_state_path(config)?;
    if !state_path.exists() {
        return Ok(None);
    }
    reject_symlink(&state_path, "OpenCode TUI keybind ownership state")?;
    let state: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&state_path)?)?;
    anyhow::ensure!(
        state["owner"] == OPENCODE_TUI_KEYBIND_STATE_OWNER,
        "{}: foreign OpenCode keybind ownership state",
        state_path.display()
    );
    let keybinds = root
        .get_mut("keybinds")
        .and_then(serde_json::Value::as_object_mut)
        .context("owned OpenCode history keybinds are missing")?;
    for key in ["history_previous", "history_next"] {
        anyhow::ensure!(
            keybinds.get(key).and_then(serde_json::Value::as_str) == Some("none"),
            "OpenCode {key} was modified after RTRT setup; preserving config and ownership state"
        );
        let prior = &state[key];
        if prior["present"].as_bool() == Some(true) {
            keybinds.insert(key.into(), prior["value"].clone());
        } else {
            keybinds.remove(key);
        }
    }
    if keybinds.is_empty() {
        root.as_object_mut()
            .expect("validated object")
            .remove("keybinds");
    }
    Ok(Some(state_path))
}

fn apply_opencode_tui_config_at(path: &Path, apply: bool, binary: &Path) -> Result<()> {
    let tuple = opencode_tui_statusline_tuple(binary, None)?;
    if !apply {
        println!("[dry-run] OpenCode TUI config target: {}", path.display());
        println!(
            "[dry-run] OpenCode TUI plugin tuple:\n{}",
            serde_json::to_string_pretty(&tuple)?
        );
        println!(
            "[dry-run] set keybinds.history_previous/history_next to none; this disables TUI history navigation, not OpenCode global history writes"
        );
        return Ok(());
    }
    reject_symlink(path, "OpenCode TUI config")?;
    let mut root = if path.exists() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        parse_json_or_jsonc(&raw, path)?
    } else {
        serde_json::json!({})
    };
    let plugin_changed = upsert_opencode_tui_statusline_plugin(&mut root, binary)?;
    let state = install_history_keybinds(&mut root, path)?;
    if !plugin_changed && state.is_none() {
        println!(
            "{}: OpenCode TUI statusline plugin already current",
            path.display()
        );
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    if path.exists() {
        backup_if_needed(path)?;
    }
    let rendered = serde_json::to_string_pretty(&root)?;
    let new_state_path = if let Some((state_path, state)) = state {
        write_private_file_atomic_same_dir(
            &state_path,
            serde_json::to_string_pretty(&state)?.as_bytes(),
        )?;
        Some(state_path)
    } else {
        None
    };
    if let Err(error) = std::fs::write(path, rendered) {
        if let Some(state_path) = new_state_path {
            let _ = std::fs::remove_file(state_path);
        }
        return Err(error).with_context(|| format!("write {}", path.display()));
    }
    println!(
        "merged OpenCode TUI statusline plugin into {}",
        path.display()
    );
    Ok(())
}

fn drop_opencode_tui_config_at(path: &Path, apply: bool) -> Result<()> {
    if !path.exists() {
        println!("{}: not present", path.display());
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would remove OpenCode TUI statusline plugin and restore owned history keybinds in {}",
            path.display()
        );
        return Ok(());
    }
    reject_symlink(path, "OpenCode TUI config")?;
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut root = parse_json_or_jsonc(&raw, path)?;
    let before = root.clone();
    let state_path = restore_history_keybinds(&mut root, path)?;
    let plugin_removed = drop_opencode_tui_statusline_plugin(&mut root)?;
    if !plugin_removed && state_path.is_none() {
        println!(
            "{}: managed OpenCode TUI statusline plugin not present",
            path.display()
        );
        return Ok(());
    }
    backup_if_needed(path)?;
    std::fs::write(path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("write {}", path.display()))?;
    if let Some(state_path) = state_path {
        if let Err(error) = std::fs::remove_file(&state_path) {
            let rollback = std::fs::write(path, serde_json::to_string_pretty(&before)?);
            if let Err(rollback) = rollback {
                bail!(
                    "remove {} failed ({error}); TUI config rollback also failed ({rollback})",
                    state_path.display()
                );
            }
            return Err(error).with_context(|| format!("remove {}", state_path.display()));
        }
    }
    println!(
        "removed OpenCode TUI statusline plugin from {}",
        path.display()
    );
    Ok(())
}

fn absolute_rtrt_binary() -> Result<PathBuf> {
    let binary = std::env::current_exe().context("resolve current rtrt executable")?;
    if !binary.is_absolute() {
        bail!(
            "cannot resolve absolute rtrt executable for OpenCode TUI statusline: {}",
            binary.display()
        );
    }
    Ok(binary)
}

fn install_opencode_tui_statusline(apply: bool) -> Result<()> {
    let root = expand_home(OPENCODE_TUI_ROOT_REL)?;
    let config = resolve_opencode_tui_config_path()?;
    let binary = absolute_rtrt_binary()?;
    install_opencode_tui_statusline_at(&root, &config, apply, &binary)
}

fn install_opencode_tui_statusline_at(
    root: &Path,
    config: &Path,
    apply: bool,
    binary: &Path,
) -> Result<()> {
    let statusline_path = root.join(OPENCODE_TUI_STATUSLINE_FILE);
    let core_path = root.join(OPENCODE_TUI_STATUSLINE_CORE_FILE);
    let statusline = managed_tui_statusline_source();
    let core = managed_tui_statusline_core_source();
    if apply {
        let _ = opencode_tui_statusline_tuple(binary, None)?;
        if config.exists() {
            let raw = std::fs::read_to_string(config)
                .with_context(|| format!("read {}", config.display()))?;
            let mut root = parse_json_or_jsonc(&raw, config)?;
            upsert_opencode_tui_statusline_plugin(&mut root, binary)?;
            install_history_keybinds(&mut root, config)?;
        }
        ensure_managed_tui_file_installable(
            &statusline_path,
            &statusline,
            OPENCODE_TUI_STATUSLINE_BEGIN,
            OPENCODE_TUI_STATUSLINE_END,
        )?;
        ensure_managed_tui_file_installable(
            &core_path,
            &core,
            OPENCODE_TUI_STATUSLINE_CORE_BEGIN,
            OPENCODE_TUI_STATUSLINE_CORE_END,
        )?;
    }
    install_managed_tui_file_at(
        &statusline_path,
        &statusline,
        OPENCODE_TUI_STATUSLINE_BEGIN,
        OPENCODE_TUI_STATUSLINE_END,
        apply,
    )?;
    install_managed_tui_file_at(
        &core_path,
        &core,
        OPENCODE_TUI_STATUSLINE_CORE_BEGIN,
        OPENCODE_TUI_STATUSLINE_CORE_END,
        apply,
    )?;
    apply_opencode_tui_config_at(config, apply, binary)
}

fn remove_opencode_tui_statusline(apply: bool) -> Result<()> {
    let root = expand_home(OPENCODE_TUI_ROOT_REL)?;
    let config = resolve_opencode_tui_config_path()?;
    remove_opencode_tui_statusline_at(&root, &config, apply)
}

fn remove_opencode_tui_statusline_at(root: &Path, config: &Path, apply: bool) -> Result<()> {
    drop_opencode_tui_config_at(config, apply)?;
    remove_managed_tui_file_at(
        &root.join(OPENCODE_TUI_STATUSLINE_FILE),
        &managed_tui_statusline_source(),
        OPENCODE_TUI_STATUSLINE_BEGIN,
        OPENCODE_TUI_STATUSLINE_END,
        apply,
    )?;
    remove_managed_tui_file_at(
        &root.join(OPENCODE_TUI_STATUSLINE_CORE_FILE),
        &managed_tui_statusline_core_source(),
        OPENCODE_TUI_STATUSLINE_CORE_BEGIN,
        OPENCODE_TUI_STATUSLINE_CORE_END,
        apply,
    )
}

fn expand_home(rel: &str) -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(expand_in_home(&home, rel))
}

/// Home-parameterized core of `expand_home`, split out so tests can resolve
/// `~/`-relative paths against a temp directory instead of the real `$HOME`.
fn expand_in_home(home: &Path, rel: &str) -> PathBuf {
    if let Some(rest) = rel.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(rel)
    }
}

pub(crate) fn dirs_home() -> Result<PathBuf> {
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

fn provenance_hook_entry(rtrt_cmd: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({
        "matcher": "*",
        "hooks": [{
            "type": COMMAND_HOOK_TYPE,
            "command": rtrt_cmd,
            "args": ["hook", "provenance", "--owner", owner],
            "timeout": HOOK_COMMAND_TIMEOUT_SECONDS
        }]
    })
}

fn is_rtrt_executable(command: &str) -> bool {
    let command = command.trim().trim_matches(['\'', '"']);
    command.rsplit(['/', '\\']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("rtrt") || name.eq_ignore_ascii_case("rtrt.exe")
    })
}

fn provenance_hook_matches(hook: &serde_json::Value, owner: &str) -> bool {
    if hook.get("type").and_then(|value| value.as_str()) != Some(COMMAND_HOOK_TYPE) {
        return false;
    }
    let Some(command) = hook.get("command").and_then(|value| value.as_str()) else {
        return false;
    };
    let expected_args = ["hook", "provenance", "--owner", owner];
    if let Some(args) = hook.get("args").and_then(|value| value.as_array()) {
        return is_rtrt_executable(command)
            && args.len() == expected_args.len()
            && args
                .iter()
                .zip(expected_args)
                .all(|(actual, expected)| actual.as_str() == Some(expected));
    }

    let suffix = format!(" hook provenance --owner {owner}");
    command
        .strip_suffix(&suffix)
        .is_some_and(is_rtrt_executable)
}

fn remove_provenance_handlers(entries: &mut Vec<serde_json::Value>, owner: &str) -> bool {
    let mut removed = false;
    entries.retain_mut(|entry| {
        let Some(hooks) = entry
            .get_mut("hooks")
            .and_then(|value| value.as_array_mut())
        else {
            return true;
        };
        let before = hooks.len();
        hooks.retain(|hook| !provenance_hook_matches(hook, owner));
        let entry_changed = hooks.len() != before;
        removed |= entry_changed;
        !entry_changed || !hooks.is_empty()
    });
    removed
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
        println!("[dry-run] provenance:  {rtrt_cmd} hook provenance (SessionStart)");
        println!("[dry-run] hook events: {} entries", HOOK_EVENTS.len());
        println!("[dry-run] style hooks: {rtrt_cmd} hook style, {rtrt_cmd} hook style-inject");
        println!(
            "[dry-run] Command Optimizer hook:\n{}",
            serde_json::to_string_pretty(&proxy_rewrite_hook_entry())
                .unwrap_or_else(|_| String::new())
        );
        println!("[dry-run] statusLine:  {rtrt_cmd} {STATUSLINE_COMMAND_SUFFIX}");
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
            remove_provenance_handlers(arr_mut, "claude");
            arr_mut.push(provenance_hook_entry(&rtrt_cmd, "claude"));
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
    root_obj.insert("statusLine".to_string(), claude_statusline_entry(&rtrt_cmd));
    let rendered = serde_json::to_string_pretty(&root)?;
    std::fs::write(&settings, rendered).with_context(|| format!("write {}", settings.display()))?;
    println!(
        "merged {} hook entries (+ auto-recall on UserPromptSubmit) into {}",
        HOOK_EVENTS.len(),
        settings.display()
    );
    Ok(())
}

fn install_claude_statusline(apply: bool) -> Result<()> {
    let settings = expand_home("~/.claude/settings.json")?;
    let rtrt_cmd = locate_rtrt_binary();
    if !apply {
        println!("[dry-run] target:      {}", settings.display());
        println!("[dry-run] statusLine:  {rtrt_cmd} {STATUSLINE_COMMAND_SUFFIX}");
        println!("Re-run with --apply to merge the statusLine entry.");
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
    root_obj.insert("statusLine".to_string(), claude_statusline_entry(&rtrt_cmd));
    let rendered = serde_json::to_string_pretty(&root)?;
    std::fs::write(&settings, rendered).with_context(|| format!("write {}", settings.display()))?;
    println!("merged rich statusLine into {}", settings.display());
    Ok(())
}

fn claude_statusline_entry(rtrt_cmd: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": format!("{rtrt_cmd} {STATUSLINE_COMMAND_SUFFIX}")
    })
}

/// Strips an rtrt-owned `statusLine` entry from a parsed settings root.
/// A user-authored statusline (command without the rtrt suffix) is preserved.
fn strip_rtrt_statusline(root: &mut serde_json::Value) -> bool {
    let is_rtrt = root
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains(STATUSLINE_COMMAND_SUFFIX));
    if is_rtrt && let Some(obj) = root.as_object_mut() {
        obj.remove("statusLine");
        return true;
    }
    false
}

/// Reverse of `install_claude_statusline`: drops the rtrt `statusLine` entry
/// from `~/.claude/settings.json` so uninstalled binaries aren't invoked on
/// every prompt render.
fn remove_claude_statusline(apply: bool) -> Result<()> {
    let settings = expand_home("~/.claude/settings.json")?;
    if !settings.exists() {
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would drop the rtrt statusLine entry from {}",
            settings.display()
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(&settings)
        .with_context(|| format!("read {}", settings.display()))?;
    if raw.trim().is_empty() {
        return Ok(());
    }
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{}: not valid JSON", settings.display()))?;
    if strip_rtrt_statusline(&mut root) {
        backup_if_needed(&settings)?;
        let rendered = serde_json::to_string_pretty(&root)?;
        std::fs::write(&settings, rendered)
            .with_context(|| format!("write {}", settings.display()))?;
        println!("dropped rtrt statusLine from {}", settings.display());
    }
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
    let mut agents_written = 0usize;
    let mut agents_skipped = 0usize;
    for agent in CLAUDE_AGENTS {
        let path = claude_agent_path(&agents_root, agent);
        if path.exists() {
            agents_skipped += 1;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        std::fs::write(&path, agent.body).with_context(|| format!("write {}", path.display()))?;
        agents_written += 1;
    }
    println!(
        "wrote {} rtrt Output Optimizer skill files under {}",
        CLAUDE_SKILLS.len(),
        skills_root.display()
    );
    println!(
        "wrote {agents_written} rtrt Output Optimizer agent files under {} ({agents_skipped} existing)",
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
        AgentKind::Opencode => Some(OPENCODE_RULES_REL),
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

#[derive(Debug)]
struct OpenCodeTaskAgentPlan {
    files: BTreeMap<String, String>,
    ollama_models: BTreeSet<String>,
    excluded_shell: Vec<(String, Option<String>)>,
}

fn install_opencode_task_agents(apply: bool, execution_confined: bool) -> Result<()> {
    let team = rtrt_core::Config::load()?.team;
    let agents_root = expand_home(OPENCODE_AGENTS_ROOT_REL)?;
    let config = resolve_opencode_config_path()?;
    install_opencode_task_agents_at_with_confinement(
        &agents_root,
        &config,
        &team,
        apply,
        execution_confined,
    )
}

#[cfg(test)]
fn install_opencode_task_agents_at(
    agents_root: &Path,
    config_path: &Path,
    team: &rtrt_core::TeamConfig,
    apply: bool,
) -> Result<()> {
    // Most unit tests exercise agent ownership and do not need an operator
    // configured Ollama provider. Keep those fixtures provider-neutral; the
    // provider gate itself is tested with the real Ollama-shaped team below.
    let team = if config_path.exists()
        && read_opencode_config(config_path)
            .ok()
            .and_then(|(_, root)| {
                root.get("provider")
                    .and_then(|value| value.get("ollama"))
                    .cloned()
            })
            .is_some()
    {
        team.clone()
    } else {
        let mut neutral = team.clone();
        if neutral.manager_provider == "ollama" {
            neutral.manager_provider = "opencode-go".to_string();
        }
        if !neutral.manager_model.contains('/') {
            neutral.manager_model =
                format!("{}/{}", neutral.manager_provider, neutral.manager_model);
        }
        for member in &mut neutral.members {
            if let Some(model) = &mut member.model
                && let Some(model_name) = model.strip_prefix("ollama/")
            {
                *model = format!("opencode-go/{model_name}");
            }
        }
        neutral
    };
    install_opencode_task_agents_at_with_confinement(agents_root, config_path, &team, apply, true)
}

fn install_opencode_task_agents_at_with_confinement(
    agents_root: &Path,
    config_path: &Path,
    team: &rtrt_core::TeamConfig,
    apply: bool,
    execution_confined: bool,
) -> Result<()> {
    if !team.enabled {
        println!(
            "{}OpenCode Task agents disabled by global [team] config",
            if apply { "" } else { "[dry-run] " }
        );
        return remove_opencode_task_agents_at(agents_root, config_path, apply);
    }
    let plan = build_opencode_task_agent_plan_with_confinement(team, execution_confined)?;
    let state_path = agents_root.join(OPENCODE_AGENT_STATE_FILE);
    let mut state = load_opencode_agent_state(&state_path)?;
    preflight_opencode_ollama_provider(config_path, &state_path, &plan, &mut state, apply)?;
    let previously_owned = opencode_state_agent_names(&state)?;
    preflight_opencode_task_agents(agents_root, &plan, &previously_owned)?;

    for (name, body) in &plan.files {
        println!(
            "{}OpenCode Task agent: {name} -> {} (model={})",
            if apply { "" } else { "[dry-run] " },
            agents_root.join(format!("{name}.md")).display(),
            opencode_agent_model(body).unwrap_or("inherit")
        );
    }
    for (name, host_agent) in &plan.excluded_shell {
        println!(
            "{}OpenCode Task agent excluded: lane={name}, delegation=CLI, host_agent={}",
            if apply { "" } else { "[dry-run] " },
            host_agent.as_deref().unwrap_or("unset")
        );
    }
    if !apply {
        sync_opencode_task_config(config_path, &state_path, &plan, &mut state, false)?;
        return Ok(());
    }

    ensure_private_opencode_agents_dir(agents_root)?;
    sync_opencode_task_config(config_path, &state_path, &plan, &mut state, true)?;
    let desired: BTreeSet<&str> = plan.files.keys().map(String::as_str).collect();
    for name in previously_owned
        .iter()
        .filter(|name| !desired.contains(name.as_str()))
    {
        let path = agents_root.join(format!("{name}.md"));
        if remove_owned_opencode_agent(&path)? {
            println!(
                "removed obsolete managed OpenCode Task agent {}",
                path.display()
            );
        }
    }
    for (name, body) in &plan.files {
        let path = agents_root.join(format!("{name}.md"));
        if path.exists() {
            let existing = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            if existing != *body {
                backup_managed_agent(&path, &existing)?;
                rtrt_core::write_private_file_atomic(&path, body.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
                println!("upgraded managed OpenCode Task agent {}", path.display());
            } else {
                set_private_file_mode(&path)?;
            }
        } else {
            rtrt_core::write_private_file_atomic(&path, body.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            println!("wrote managed OpenCode Task agent {}", path.display());
        }
    }
    Ok(())
}

fn remove_opencode_task_agents_at(
    agents_root: &Path,
    config_path: &Path,
    apply: bool,
) -> Result<()> {
    let state_path = agents_root.join(OPENCODE_AGENT_STATE_FILE);
    let mut state = load_opencode_agent_state(&state_path)?;
    let config_path = preferred_opencode_config_path(&state, config_path)?;
    let managed = opencode_state_agent_names(&state)?;
    if !apply {
        for name in &managed {
            let path = agents_root.join(format!("{name}.md"));
            println!(
                "[dry-run] would remove managed OpenCode Task agent {}",
                path.display()
            );
        }
        if state_path.exists() {
            println!(
                "[dry-run] would restore rtrt-owned OpenCode Task/model settings in {}",
                config_path.display()
            );
        }
        return Ok(());
    }

    let mut errors = Vec::new();
    if state_path.exists()
        && let Err(error) = restore_opencode_task_config(&config_path, &mut state)
    {
        errors.push(format!("restore {}: {error}", config_path.display()));
    }
    for name in managed {
        let path = agents_root.join(format!("{name}.md"));
        let existed = path_metadata(&path)?.is_some();
        match remove_owned_opencode_agent(&path) {
            Ok(true) => println!("removed managed OpenCode Task agent {}", path.display()),
            Ok(false) if existed => errors.push(format!(
                "remove {}: owned execution agent remains because content is modified",
                path.display()
            )),
            Ok(false) => {}
            Err(error) => errors.push(format!("remove {}: {error}", path.display())),
        }
    }
    if errors.is_empty() && state_path.exists() {
        std::fs::remove_file(&state_path)
            .with_context(|| format!("remove {}", state_path.display()))?;
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "OpenCode Task-agent uninstall encountered {} error(s): {}",
            errors.len(),
            errors.join("; ")
        )
    }
}

#[cfg(test)]
fn build_opencode_task_agent_plan(team: &rtrt_core::TeamConfig) -> Result<OpenCodeTaskAgentPlan> {
    build_opencode_task_agent_plan_with_confinement(team, true)
}

fn build_opencode_task_agent_plan_with_confinement(
    team: &rtrt_core::TeamConfig,
    execution_confined: bool,
) -> Result<OpenCodeTaskAgentPlan> {
    team.validate()?;
    let mut files = BTreeMap::new();
    let mut task_agents = BTreeSet::from(["explore".to_string()]);
    let mut excluded_shell = Vec::new();
    let manager_model = opencode_manager_model(team)?;
    let mut native_hosts = BTreeMap::new();
    let mut configured_hosts = BTreeSet::new();
    for member in &team.members {
        if member.delegation == rtrt_core::Delegation::Native
            && let Some(host) = member.host_agent.as_deref()
        {
            native_hosts.insert(member.name.as_str(), host);
        }
    }

    for member in &team.members {
        if member.delegation == rtrt_core::Delegation::Shell {
            if member
                .model
                .as_deref()
                .is_none_or(|model| model.trim().is_empty())
            {
                bail!(
                    "OpenCode CLI lane {} requires a model for direct claude -p invocation",
                    member.name
                );
            }
            excluded_shell.push((member.name.clone(), member.host_agent.clone()));
            continue;
        }
        let Some(host_agent) = member.host_agent.as_deref() else {
            continue;
        };
        validate_opencode_agent_name(host_agent)?;
        if host_agent == OPENCODE_MANAGER_AGENT {
            bail!(
                "team member {} uses reserved OpenCode agent name {OPENCODE_MANAGER_AGENT}",
                member.name
            );
        }
        if !configured_hosts.insert(host_agent) {
            bail!("duplicate OpenCode host_agent in team config: {host_agent}");
        }
        task_agents.insert(host_agent.to_string());
        if matches!(host_agent, "build" | "plan") {
            bail!(
                "team member {} maps primary-only OpenCode agent {host_agent}; worker host_agent must be a subagent",
                member.name
            );
        }
        if is_builtin_opencode_subagent(host_agent) {
            if member.model.is_some() {
                bail!(
                    "team member {} maps built-in OpenCode agent {host_agent} and cannot set model",
                    member.name
                );
            }
            continue;
        }
        let model = opencode_member_model(member);
        files.insert(
            host_agent.to_string(),
            render_opencode_worker_agent(
                member,
                model.as_deref(),
                team.policy.worker_summary_max_lines,
                execution_confined,
            ),
        );
    }
    files.insert(
        OPENCODE_MANAGER_AGENT.to_string(),
        render_opencode_manager_agent(
            team,
            &manager_model,
            &task_agents,
            &native_hosts,
            execution_confined,
        ),
    );

    let mut ollama_models = BTreeSet::new();
    for body in files.values() {
        if let Some(model) = opencode_agent_model(body)
            && let Some(model) = model.strip_prefix("ollama/")
        {
            ollama_models.insert(model.to_string());
        }
    }
    Ok(OpenCodeTaskAgentPlan {
        files,
        ollama_models,
        excluded_shell,
    })
}

fn opencode_manager_model(team: &rtrt_core::TeamConfig) -> Result<String> {
    let provider = team.manager_provider.trim();
    let model = team.manager_model.trim();
    if provider.is_empty() || model.is_empty() {
        bail!("OpenCode manager provider/model must not be empty");
    }
    if model.contains('/') {
        Ok(model.to_string())
    } else {
        Ok(format!("{provider}/{model}"))
    }
}

fn opencode_member_model(member: &rtrt_core::TeamMember) -> Option<String> {
    let model = member.model.as_deref()?.trim();
    if model.is_empty() {
        return None;
    }
    if model.contains('/') || member.target == "opencode" {
        Some(model.to_string())
    } else {
        Some(format!("{}/{model}", member.target))
    }
}

fn validate_opencode_agent_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!(
            "invalid OpenCode host_agent {name:?}: expected only ASCII letters, digits, '-' or '_'"
        );
    }
    Ok(())
}

fn is_builtin_opencode_subagent(name: &str) -> bool {
    matches!(name, "explore" | "general" | "scout")
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn shell_quote_arg(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'@' | b'+' | b',')
        })
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn claude_shell_argv_template(member: &rtrt_core::TeamMember) -> String {
    let mut argv = vec![
        "claude".to_string(),
        "-p".to_string(),
        "--model".to_string(),
        shell_quote_arg(member.model.as_deref().unwrap_or_default()),
    ];
    for (key, value) in &member.flags {
        if key.eq_ignore_ascii_case(CLAUDE_PERMISSION_PROMPT_FLAG) {
            continue;
        }
        argv.push(shell_quote_arg(&format!("--{key}")));
        if !value.is_empty() {
            argv.push(shell_quote_arg(value));
        }
    }
    argv.push(format!("--{CLAUDE_PERMISSION_PROMPT_FLAG}"));
    argv.push(CLAUDE_PERMISSION_PROMPT_TOOL.to_string());
    argv.push("'<PROMPT>'".to_string());
    argv.join(" ")
}

const OPENCODE_MANAGER_PHASE_POLICY: &str = r#"Classify and route work only from the rendered configuration: lane roles, effective tiers, design-only tiers, review tier, leader order, sibling links, fallback routes, balance policy, and delegation mode. Source code does not impose model-name shortcuts or mandatory plan/review phases. Select a configured plan or review lane only when its configured roles and tier fit the task; a design-only tier may inspect and design but must not implement.

Use built-in `explore` for discovery. Invoke every selected lane through its rendered delegation mechanism: native Task for delegation=Native or direct `claude -p` for delegation=CLI. When a CLI lane has `output-format=json`, parse its JSON result rather than treating the envelope as prose.

For ordinary transient failures, retry the same lane at most `max_retries` times, then walk rendered `fallback_routes` in configured order up to `max_fallback_depth`; honor `redo_on_fallback`. The installed RTRT OpenCode plugin recognizes exactly three native-child provider limits: structured `429` with `rate_limit`, structured `529` with `capacity`, and provider transport `weekly_usage_limit` only when `session.next.retried` contains the exact machine code `weekly_usage_limit` or exact phrase `weekly usage limit` / `weekly usage cap`. When a native Task returns interrupted/aborted from any of those circuit breakers, mark the exact selected lane unavailable for the current session, consume zero same-lane retries, and immediately follow the configured sibling/fallback route: `sibling_native_host` first when present and `prefer_sibling_on_quota=true`, then `fallback_routes`. A recognized weekly limit also consumes zero same-lane retries, excludes the exact lane for the current session, and follows the configured sibling then fallback route. Do not classify generic text, generic 5xx, authentication failures, user cancellation, or ordinary transient errors as quota. Never use the sibling first for ordinary failures. On authentication failure, report the failure and move only according to rendered fallback policy. Preserve configured tier/lane order whenever no trustworthy room signal exists; `balance=room` may reorder equally suitable lanes only when a room signal exists."#;

fn render_default_opencode_bash_rules(execution_confined: bool) -> String {
    format!(
        "  bash:\n    \"*\": {}\n",
        if execution_confined { "allow" } else { "deny" }
    )
}

fn render_opencode_manager_agent(
    team: &rtrt_core::TeamConfig,
    model: &str,
    task_agents: &BTreeSet<String>,
    native_hosts: &BTreeMap<&str, &str>,
    execution_confined: bool,
) -> String {
    let mut task_rules = String::from("    \"*\": deny\n");
    for agent in task_agents {
        if agent != OPENCODE_MANAGER_AGENT {
            task_rules.push_str(&format!("    {}: allow\n", yaml_string(agent)));
        }
    }
    let mut bash_rules = render_default_opencode_bash_rules(execution_confined);
    for member in &team.members {
        if member.delegation == rtrt_core::Delegation::Shell {
            let pattern = claude_shell_argv_template(member).replace("'<PROMPT>'", "'*'");
            bash_rules.push_str(&format!(
                "    {}: {}\n",
                yaml_string(&pattern),
                if execution_confined { "allow" } else { "deny" }
            ));
        }
    }
    let fallback_route = |name: &str| {
        team.member(name)
            .and_then(|member| match member.delegation {
                rtrt_core::Delegation::Native => native_hosts
                    .get(name)
                    .map(|host| format!("{} => Task({})", member.name, host)),
                rtrt_core::Delegation::Shell => Some(format!(
                    "{} => {}",
                    member.name,
                    claude_shell_argv_template(member)
                )),
            })
    };
    let mut routes = String::new();
    for member in &team.members {
        if member.delegation != rtrt_core::Delegation::Native {
            continue;
        }
        let Some(host_agent) = member.host_agent.as_deref() else {
            continue;
        };
        let fallback_routes: Vec<String> = member
            .fallback
            .iter()
            .filter_map(|name| fallback_route(name))
            .collect();
        let sibling_native_host = member
            .sibling
            .as_deref()
            .and_then(|name| native_hosts.get(name).copied());
        routes.push_str(&format!(
            "- lane={} host={} roles={} sibling_native_host={} fallback_routes={}\n",
            yaml_string(&member.name),
            yaml_string(host_agent),
            serde_json::to_string(&member.roles).unwrap_or_else(|_| "[]".to_string()),
            serde_json::to_string(&sibling_native_host).unwrap_or_else(|_| "null".to_string()),
            serde_json::to_string(&fallback_routes).unwrap_or_else(|_| "[]".to_string())
        ));
    }
    let mut shell_lanes = String::new();
    for member in &team.members {
        if member.delegation == rtrt_core::Delegation::Shell {
            let fallback_routes: Vec<String> = member
                .fallback
                .iter()
                .filter_map(|name| fallback_route(name))
                .collect();
            shell_lanes.push_str(&format!(
                "- lane={} model={} roles={} flags={} delegation=CLI argv_template={} fallback_routes={}\n",
                yaml_string(&member.name),
                yaml_string(member.model.as_deref().unwrap_or_default()),
                serde_json::to_string(&member.roles).unwrap_or_else(|_| "[]".to_string()),
                serde_json::to_string(&member.flags).unwrap_or_else(|_| "{}".to_string()),
                yaml_string(&claude_shell_argv_template(member)),
                serde_json::to_string(&fallback_routes).unwrap_or_else(|_| "[]".to_string())
            ));
        }
    }
    let effective_tiers = team.effective_tiers();
    let mut tiers = String::new();
    for (tier, members) in effective_tiers.iter() {
        let lanes: Vec<String> = members
            .iter()
            .map(|name| {
                if let Some(host) = native_hosts.get(name.as_str()) {
                    (*host).to_string()
                } else if let Some(member) = team.member(name)
                    && member.delegation == rtrt_core::Delegation::Shell
                {
                    claude_shell_argv_template(member)
                } else {
                    format!("unavailable:{name}")
                }
            })
            .collect();
        tiers.push_str(&format!(
            "- {}: {}\n",
            yaml_string(tier),
            serde_json::to_string(&lanes).unwrap_or_else(|_| "[]".to_string())
        ));
    }
    let design_only_tiers: Vec<&str> = effective_tiers
        .names()
        .filter(|tier| team.is_design_only_tier(tier))
        .collect();
    let default_tier = team.effective_default_tier();
    let balance = match team.policy.balance {
        rtrt_core::Balance::Order => "order",
        rtrt_core::Balance::Room => "room",
    };
    format!(
        r#"---
description: {}
mode: primary
model: {}
permission:
  read: allow
  edit: allow
  glob: allow
  grep: allow
  list: allow
  task:
{task_rules}{bash_rules}  external_directory: ask
  rtrt_agent_call: deny
  rtrt_agent_route: deny
  rtrt_team_dispatch: deny
---
{OPENCODE_AGENT_BEGIN}
Coordinate implementation through OpenCode native Task and configured direct Claude CLI lanes. Use native Task only for delegation=Native lanes. Use direct `claude -p` only for delegation=CLI lanes. Bash is enabled only when setup verified strict confinement; otherwise every Bash pattern is denied. Outside-project access requires OpenCode's native external-directory permission. Respect rejection and cancellation immediately. Fixed Task recursion and RTRT agent/team bridge permissions remain denied. OpenCode `once` is one request. RTRT Claude bridge `always` is parent-session/process scoped and clears on session deletion or OpenCode restart. RTRT does not persist raw approvals or project-saved permission rules. Never wrap or obfuscate commands to evade matching, launch nested OpenCode, or invoke RTRT agent/team bridges.

For CLI lanes, copy the rendered argv template exactly and replace `<PROMPT>` with one POSIX single-quoted final argument, escaping each embedded `'` as `'"'"'`. Preserve the original task prompt semantically; never add shell operators (`|`, `&`, `;`, `<`, `>`), redirects, command substitutions, wrappers, environment prefixes, or extra commands. Never interpolate a task prompt into this agent file. Claude CLI lanes may not omit or replace `--permission-prompt-tool mcp__rtrt__permission_prompt`, and may not add `--allowed-tools`, `--allowedTools`, `--dangerously-skip-permissions`, or `--permission-mode bypassPermissions`.

Direct edits are limited to tiny, self-contained changes touching at most two files. Delegate larger work. Keep delegated write sets disjoint; serialize overlapping writes.

Rendered native lanes:
{routes}Rendered CLI lanes (available only through direct `claude -p`):
{shell_lanes}Effective tiers (configured executable order):
{tiers}Effective routing policy:
- leader_order={}
- default_tier={}; explore_tier={}; review_tier={}; design_only_tiers={}
- max_retries={}; redo_on_fallback={}; max_fallback_depth={}; prefer_sibling_on_quota={}; balance={}; record_provenance={}

{OPENCODE_MANAGER_PHASE_POLICY}
{OPENCODE_AGENT_END}
"#,
        yaml_string(
            "RTRT team manager. Routes through native OpenCode Task agents or configured direct Claude CLI lanes and performs tiny edits."
        ),
        yaml_string(model),
        serde_json::to_string(&team.leader_order).unwrap_or_else(|_| "[]".to_string()),
        serde_json::to_string(&default_tier).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&team.policy.explore_tier).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&team.policy.review_tier).unwrap_or_else(|_| "null".to_string()),
        serde_json::to_string(&design_only_tiers).unwrap_or_else(|_| "[]".to_string()),
        team.policy.max_retries,
        team.policy.redo_on_fallback,
        team.effective_max_fallback_depth(),
        team.policy.prefer_sibling_on_quota,
        yaml_string(balance),
        team.policy.record_provenance,
    )
}

fn render_opencode_worker_agent(
    member: &rtrt_core::TeamMember,
    model: Option<&str>,
    summary_max_lines: u8,
    execution_confined: bool,
) -> String {
    let permissions = render_opencode_worker_permissions(member, execution_confined);
    let model_line = model
        .map(|model| format!("model: {}\n", yaml_string(model)))
        .unwrap_or_default();
    let roles = serde_json::to_string(&member.roles).unwrap_or_else(|_| "[]".to_string());
    let fallback = serde_json::to_string(&member.fallback).unwrap_or_else(|_| "[]".to_string());
    format!(
        r#"---
description: {}
mode: subagent
{model_line}permission:
{permissions}  task: deny
  external_directory: ask
  rtrt_agent_call: deny
  rtrt_agent_route: deny
  rtrt_team_dispatch: deny
---
{OPENCODE_AGENT_BEGIN}
Work within current project by default: project-internal reads and permitted edits are allowed; outside-project resources require OpenCode native permission. Bash is enabled only when setup verified strict confinement; otherwise every Bash pattern is denied. Never invoke Task, nested agents, OpenCode processes, or RTRT agent/team bridges. OpenCode `once` is one request. RTRT Claude bridge `always` is parent-session/process scoped and clears on session deletion or OpenCode restart. RTRT does not persist raw approvals or project-saved permission rules. Use project `.rtrt/tmp` for temporary files.

Configured lane: {}. Roles: {roles}. Fallback lanes: {fallback}. Fallback execution belongs to the manager; if blocked, return a concise failure for reassignment.

{}
Return at most {} summary lines with changed files and verification.
{OPENCODE_AGENT_END}
"#,
        yaml_string(&format!("RTRT native worker for lane {}.", member.name)),
        yaml_string(&member.name),
        "Follow the rendered permission policy and host defaults. Project-internal reads, edits, and writes are allowed; outside-project reads, edits, and writes require OpenCode native permission.",
        summary_max_lines
    )
}

fn permission_action_name(action: &rtrt_core::config::PermissionAction) -> &'static str {
    match action {
        rtrt_core::config::PermissionAction::Allow => "allow",
        rtrt_core::config::PermissionAction::Ask => "ask",
        rtrt_core::config::PermissionAction::Deny => "deny",
    }
}

fn render_opencode_worker_permissions(
    member: &rtrt_core::TeamMember,
    execution_confined: bool,
) -> String {
    let mut rendered = String::from("  read: allow\n  glob: allow\n  grep: allow\n  list: allow\n");
    if !member.allow_impl {
        rendered.push_str("  edit: deny\n");
    } else if let Some(edit) = member
        .permissions
        .as_ref()
        .and_then(|permissions| permissions.edit.as_ref())
    {
        rendered.push_str(&format!("  edit: {}\n", permission_action_name(edit)));
    } else {
        rendered.push_str("  edit: allow\n");
    }

    rendered.push_str(&render_default_opencode_bash_rules(execution_confined));
    if let Some(bash) = member
        .permissions
        .as_ref()
        .map(|permissions| &permissions.bash)
        .filter(|bash| !bash.is_empty())
    {
        for (pattern, action) in bash.iter() {
            rendered.push_str(&format!(
                "    {}: {}\n",
                yaml_string(pattern),
                if execution_confined {
                    permission_action_name(&action)
                } else {
                    "deny"
                }
            ));
        }
    }
    rendered
}

fn opencode_agent_model(body: &str) -> Option<&str> {
    body.lines()
        .find_map(|line| line.strip_prefix("model: "))
        .and_then(|value| serde_json::from_str::<&str>(value).ok())
}

fn managed_opencode_agent(raw: &str) -> bool {
    let Some(begin) = exact_marker_line_range(raw, OPENCODE_AGENT_BEGIN) else {
        return false;
    };
    let Some(end) = exact_marker_line_range(raw, OPENCODE_AGENT_END) else {
        return false;
    };
    raw.starts_with("---\n") && begin.start < end.start && raw[end.end..].trim().is_empty()
}

fn preflight_opencode_task_agents(
    root: &Path,
    plan: &OpenCodeTaskAgentPlan,
    previously_owned: &BTreeSet<String>,
) -> Result<()> {
    if let Some(metadata) = path_metadata(root)? {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "{}: OpenCode agents root is not a real directory",
                root.display()
            );
        }
    }
    for name in plan.files.keys() {
        let path = root.join(format!("{name}.md"));
        let Some(metadata) = path_metadata(&path)? else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "{}: refusing to overwrite non-file or symlink at managed agent name",
                path.display()
            );
        }
        if !previously_owned.contains(name) {
            bail!(
                "{}: refusing to overwrite unknown same-name OpenCode agent without private ownership state",
                path.display()
            );
        }
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if !managed_opencode_agent(&raw) {
            bail!(
                "{}: refusing to overwrite unknown same-name OpenCode agent",
                path.display()
            );
        }
        reject_symlink(&backup_path(&path), "managed agent backup")?;
    }
    Ok(())
}

fn remove_owned_opencode_agent(path: &Path) -> Result<bool> {
    let Some(metadata) = path_metadata(path)? else {
        return Ok(false);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{}: refusing to remove non-file or symlink at owned agent name",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if !managed_opencode_agent(&raw) {
        println!(
            "{}: owned name no longer has exact RTRT markers; preserving it",
            path.display()
        );
        return Ok(false);
    }
    reject_symlink(&backup_path(path), "managed agent backup")?;
    std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    remove_managed_agent_backup(path)?;
    Ok(true)
}

fn ensure_private_opencode_agents_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("mkdir {}", path.display()))?;
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "{}: OpenCode agents root is not a real directory",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

fn set_private_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    Ok(())
}

fn backup_managed_agent(path: &Path, existing: &str) -> Result<()> {
    let backup = backup_path(path);
    reject_symlink(&backup, "managed agent backup")?;
    if !backup.exists() {
        rtrt_core::write_private_file_atomic(&backup, existing.as_bytes())
            .with_context(|| format!("backup {}", backup.display()))?;
    }
    Ok(())
}

fn remove_managed_agent_backup(path: &Path) -> Result<()> {
    let backup = backup_path(path);
    let Some(metadata) = path_metadata(&backup)? else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{}: refusing managed agent backup destination",
            backup.display()
        );
    }
    let raw =
        std::fs::read_to_string(&backup).with_context(|| format!("read {}", backup.display()))?;
    if managed_opencode_agent(&raw) {
        std::fs::remove_file(&backup).with_context(|| format!("remove {}", backup.display()))?;
    }
    Ok(())
}

fn new_opencode_agent_state() -> serde_json::Value {
    serde_json::json!({
        "owner": OPENCODE_AGENT_STATE_OWNER,
        "version": 1,
        "models": {}
    })
}

fn load_opencode_agent_state(path: &Path) -> Result<serde_json::Value> {
    let Some(metadata) = path_metadata(path)? else {
        return Ok(new_opencode_agent_state());
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{}: refusing unknown OpenCode agent state file",
            path.display()
        );
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let state: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{}: invalid managed state JSON", path.display()))?;
    if state.get("owner").and_then(serde_json::Value::as_str) != Some(OPENCODE_AGENT_STATE_OWNER)
        || state.get("version").and_then(serde_json::Value::as_u64) != Some(1)
        || !state.is_object()
    {
        bail!(
            "{}: refusing unknown OpenCode agent state file",
            path.display()
        );
    }
    Ok(state)
}

fn write_opencode_agent_state(path: &Path, state: &serde_json::Value) -> Result<()> {
    reject_symlink(path, "OpenCode agent ownership state")?;
    let rendered = serde_json::to_string_pretty(state)?;
    rtrt_core::write_private_file_atomic(path, rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

fn opencode_state_agent_names(state: &serde_json::Value) -> Result<BTreeSet<String>> {
    let Some(agents) = state.get("agents") else {
        return Ok(BTreeSet::new());
    };
    let agents = agents
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("OpenCode managed state agents is not an array"))?;
    let mut names = BTreeSet::new();
    for value in agents {
        let name = value
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("OpenCode managed state agent name is not a string"))?;
        validate_opencode_agent_name(name)?;
        if !names.insert(name.to_string()) {
            bail!("OpenCode managed state lists duplicate agent {name}");
        }
    }
    Ok(names)
}

fn preferred_opencode_config_path(state: &serde_json::Value, fallback: &Path) -> Result<PathBuf> {
    let Some(recorded) = state.get("config_path") else {
        return Ok(fallback.to_path_buf());
    };
    let recorded = recorded
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("OpenCode managed state config_path is not a string"))?;
    let parent = fallback
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{}: OpenCode config has no parent", fallback.display()))?;
    let json = parent.join("opencode.json");
    let jsonc = parent.join("opencode.jsonc");
    let recorded = PathBuf::from(recorded);
    if recorded != json && recorded != jsonc {
        bail!(
            "OpenCode managed state config_path {} is outside the resolved home config paths",
            recorded.display()
        );
    }
    Ok(recorded)
}

fn optional_json_value(value: Option<&serde_json::Value>) -> serde_json::Value {
    match value {
        Some(value) => serde_json::json!({"present": true, "value": value}),
        None => serde_json::json!({"present": false}),
    }
}

fn restore_optional_json_value(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    record: &serde_json::Value,
) {
    if record
        .get("present")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(value) = record.get("value") {
            object.insert(key.to_string(), value.clone());
        }
    } else {
        object.remove(key);
    }
}

fn sync_opencode_task_config(
    path: &Path,
    state_path: &Path,
    plan: &OpenCodeTaskAgentPlan,
    state: &mut serde_json::Value,
    apply: bool,
) -> Result<()> {
    let (raw, mut root) = read_opencode_config(path)?;
    let before = root.clone();
    restore_all_opencode_task_permissions(&mut root, state)?;
    sync_opencode_ollama_models(&mut root, &plan.ollama_models, state)?;
    sync_opencode_subagent_depth(&mut root, state)?;
    println!(
        "{}OpenCode config: {} (provenance plugin=auto-loaded, ollama tool_call models={}, global Task policy unchanged, subagent_depth>=1)",
        if apply { "" } else { "[dry-run] " },
        path.display(),
        plan.ollama_models
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    );
    if apply {
        let state_object = state
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("OpenCode managed state root is not an object"))?;
        let exact_config_path = path.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "{}: OpenCode config path is not valid UTF-8; cannot persist exact ownership",
                path.display()
            )
        })?;
        state_object.insert(
            "config_path".to_string(),
            serde_json::Value::String(exact_config_path.to_string()),
        );
        state_object.insert(
            "agents".to_string(),
            serde_json::Value::Array(
                plan.files
                    .keys()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        write_opencode_agent_state(state_path, state)?;
        if root != before {
            write_opencode_config(path, &raw, &before, &root)?;
        }
    }
    Ok(())
}

fn restore_opencode_task_config(path: &Path, state: &mut serde_json::Value) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let (raw, mut root) = read_opencode_config(path)?;
    let before = root.clone();
    restore_all_opencode_ollama_models(&mut root, state)?;
    restore_all_opencode_task_permissions(&mut root, state)?;
    restore_opencode_subagent_depth(&mut root, state);
    cleanup_opencode_created_config_objects(&mut root, state);
    if root != before {
        write_opencode_config(path, &raw, &before, &root)?;
        println!(
            "restored rtrt-owned OpenCode Task/model settings in {}",
            path.display()
        );
    }
    Ok(())
}

fn read_opencode_config(path: &Path) -> Result<(String, serde_json::Value)> {
    let Some(metadata) = path_metadata(path)? else {
        return Ok(("{}".to_string(), serde_json::json!({})));
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{}: OpenCode config is not a real file", path.display());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root = parse_json_or_jsonc(&raw, path)?;
    Ok((raw, root))
}

fn write_opencode_config(
    path: &Path,
    raw: &str,
    before: &serde_json::Value,
    root: &serde_json::Value,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    if path.exists() {
        backup_if_needed(path)?;
    }
    let rendered = apply_jsonc_object_diff(raw, before, root)
        .with_context(|| format!("surgically merge JSONC in {}", path.display()))?;
    let reparsed = parse_json_or_jsonc(&rendered, path)?;
    if &reparsed != root {
        bail!(
            "{}: surgical JSONC merge did not produce expected config; refusing to write",
            path.display()
        );
    }
    write_private_file_atomic_same_dir(path, rendered.as_bytes())?;
    Ok(())
}

#[derive(Debug)]
struct JsoncObjectMember {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

#[derive(Debug)]
struct JsoncChange {
    path: Vec<String>,
    value: Option<serde_json::Value>,
}

fn apply_jsonc_object_diff(
    raw: &str,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Result<String> {
    let mut changes = Vec::new();
    collect_jsonc_changes(before, after, &mut Vec::new(), &mut changes);
    let mut rendered = raw.to_string();
    for change in changes {
        if change.path.is_empty() {
            bail!("OpenCode config root replacement is not supported");
        }
        rendered = match change.value {
            Some(value) => jsonc_set_path(&rendered, &change.path, &value)?,
            None => jsonc_remove_path(&rendered, &change.path)?,
        };
    }
    Ok(rendered)
}

fn collect_jsonc_changes(
    before: &serde_json::Value,
    after: &serde_json::Value,
    path: &mut Vec<String>,
    changes: &mut Vec<JsoncChange>,
) {
    if before == after {
        return;
    }
    match (before.as_object(), after.as_object()) {
        (Some(before), Some(after)) => {
            let keys: BTreeSet<&str> = before
                .keys()
                .chain(after.keys())
                .map(String::as_str)
                .collect();
            for key in keys {
                path.push(key.to_string());
                match (before.get(key), after.get(key)) {
                    (Some(before), Some(after)) => {
                        collect_jsonc_changes(before, after, path, changes);
                    }
                    (None, Some(value)) => changes.push(JsoncChange {
                        path: path.clone(),
                        value: Some(value.clone()),
                    }),
                    (Some(_), None) => changes.push(JsoncChange {
                        path: path.clone(),
                        value: None,
                    }),
                    (None, None) => {}
                }
                path.pop();
            }
        }
        _ => changes.push(JsoncChange {
            path: path.clone(),
            value: Some(after.clone()),
        }),
    }
}

fn jsonc_root_open(raw: &str) -> Result<usize> {
    let mut cursor = 0usize;
    skip_jsonc_trivia(raw, &mut cursor, raw.len())?;
    if raw.as_bytes().get(cursor) != Some(&b'{') {
        bail!("OpenCode config root is not a JSON object");
    }
    Ok(cursor)
}

fn jsonc_set_path(raw: &str, path: &[String], value: &serde_json::Value) -> Result<String> {
    let mut object_open = jsonc_root_open(raw)?;
    for (index, key) in path.iter().enumerate() {
        let last = index + 1 == path.len();
        let members = jsonc_object_members(raw, object_open)?;
        if let Some(member) = members.iter().find(|member| member.key == *key) {
            if last {
                let value = serde_json::to_string(value)?;
                let mut out = raw.to_string();
                out.replace_range(member.value_start..member.value_end, &value);
                return Ok(out);
            }
            if raw.as_bytes().get(member.value_start) != Some(&b'{') {
                bail!(
                    "OpenCode config {} is not an object",
                    path[..=index].join(".")
                );
            }
            object_open = member.value_start;
            continue;
        }

        let nested = path[index + 1..]
            .iter()
            .rev()
            .fold(value.clone(), |value, key| serde_json::json!({key: value}));
        return jsonc_insert_object_member(raw, object_open, key, &nested);
    }
    Ok(raw.to_string())
}

fn jsonc_remove_path(raw: &str, path: &[String]) -> Result<String> {
    let mut object_open = jsonc_root_open(raw)?;
    for (index, key) in path.iter().enumerate() {
        let members = jsonc_object_members(raw, object_open)?;
        let Some(position) = members.iter().position(|member| member.key == *key) else {
            return Ok(raw.to_string());
        };
        let member = &members[position];
        if index + 1 != path.len() {
            if raw.as_bytes().get(member.value_start) != Some(&b'{') {
                return Ok(raw.to_string());
            }
            object_open = member.value_start;
            continue;
        }

        let object_close = find_matching_brace(raw, object_open)
            .ok_or_else(|| anyhow::anyhow!("unterminated JSONC object"))?;
        let mut after = member.value_end;
        skip_jsonc_trivia(raw, &mut after, object_close)?;
        let mut out = raw.to_string();
        if raw.as_bytes().get(after) == Some(&b',') {
            out.replace_range(member.key_start..after + 1, "");
        } else if position > 0 {
            let mut comma = members[position - 1].value_end;
            skip_jsonc_trivia(raw, &mut comma, member.key_start)?;
            if raw.as_bytes().get(comma) == Some(&b',') {
                out.replace_range(member.key_start..member.value_end, "");
                out.remove(comma);
            } else {
                out.replace_range(member.key_start..member.value_end, "");
            }
        } else {
            out.replace_range(member.key_start..member.value_end, "");
        }
        return Ok(out);
    }
    Ok(raw.to_string())
}

fn jsonc_insert_object_member(
    raw: &str,
    object_open: usize,
    key: &str,
    value: &serde_json::Value,
) -> Result<String> {
    let members = jsonc_object_members(raw, object_open)?;
    let object_close = find_matching_brace(raw, object_open)
        .ok_or_else(|| anyhow::anyhow!("unterminated JSONC object"))?;
    let mut out = raw.to_string();
    if let Some(last) = members.last() {
        let mut comma = last.value_end;
        skip_jsonc_trivia(raw, &mut comma, object_close)?;
        if raw.as_bytes().get(comma) != Some(&b',') {
            out.insert(last.value_end, ',');
        }
    }

    let object_close = find_matching_brace(&out, object_open)
        .ok_or_else(|| anyhow::anyhow!("unterminated JSONC object"))?;
    let line_start = out[..object_open]
        .rfind('\n')
        .map_or(0, |position| position + 1);
    let base_indent: String = out[line_start..object_open]
        .chars()
        .take_while(|character| character.is_ascii_whitespace())
        .collect();
    let member_indent = format!("{base_indent}  ");
    let close_line = out[..object_close]
        .rfind('\n')
        .map_or(object_close, |position| position + 1);
    let insertion_at = if out[close_line..object_close].trim().is_empty() {
        close_line
    } else {
        object_close
    };
    let prefix = if out[..insertion_at].ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let rendered_key = serde_json::to_string(key)?;
    let rendered_value = serde_json::to_string(value)?;
    let insertion =
        format!("{prefix}{member_indent}{rendered_key}: {rendered_value}\n{base_indent}");
    out.insert_str(insertion_at, &insertion);
    Ok(out)
}

fn jsonc_object_members(raw: &str, object_open: usize) -> Result<Vec<JsoncObjectMember>> {
    if raw.as_bytes().get(object_open) != Some(&b'{') {
        bail!("JSONC object does not start with '{{'");
    }
    let object_close = find_matching_brace(raw, object_open)
        .ok_or_else(|| anyhow::anyhow!("unterminated JSONC object"))?;
    let mut cursor = object_open + 1;
    let mut members = Vec::new();
    loop {
        skip_jsonc_trivia(raw, &mut cursor, object_close)?;
        if raw.as_bytes().get(cursor) == Some(&b',') {
            cursor += 1;
            continue;
        }
        if cursor >= object_close {
            break;
        }
        let key_start = cursor;
        let (key, key_end) = parse_jsonc_string(raw, cursor, object_close)?;
        cursor = key_end;
        skip_jsonc_trivia(raw, &mut cursor, object_close)?;
        if raw.as_bytes().get(cursor) != Some(&b':') {
            bail!("JSONC object key {key:?} is missing ':'");
        }
        cursor += 1;
        skip_jsonc_trivia(raw, &mut cursor, object_close)?;
        let value_start = cursor;
        let value_end = jsonc_value_end(raw, value_start, object_close)?;
        members.push(JsoncObjectMember {
            key,
            key_start,
            value_start,
            value_end,
        });
        cursor = value_end;
    }
    Ok(members)
}

fn parse_jsonc_string(raw: &str, start: usize, limit: usize) -> Result<(String, usize)> {
    if raw.as_bytes().get(start) != Some(&b'"') {
        bail!("JSONC object member key is not a string");
    }
    let bytes = raw.as_bytes();
    let mut cursor = start + 1;
    let mut escaped = false;
    while cursor < limit {
        match bytes[cursor] {
            b'"' if !escaped => {
                let end = cursor + 1;
                let key = serde_json::from_str(&raw[start..end])?;
                return Ok((key, end));
            }
            b'\\' if !escaped => escaped = true,
            _ => escaped = false,
        }
        cursor += 1;
    }
    bail!("unterminated JSONC string")
}

fn jsonc_value_end(raw: &str, start: usize, limit: usize) -> Result<usize> {
    match raw.as_bytes().get(start) {
        Some(b'{') => find_matching_brace(raw, start)
            .map(|end| end + 1)
            .ok_or_else(|| anyhow::anyhow!("unterminated JSONC object value")),
        Some(b'[') => find_matching_jsonc_container(raw, start, b'[', b']')
            .map(|end| end + 1)
            .ok_or_else(|| anyhow::anyhow!("unterminated JSONC array value")),
        Some(b'"') => parse_jsonc_string(raw, start, limit).map(|(_, end)| end),
        Some(_) => {
            let bytes = raw.as_bytes();
            let mut cursor = start;
            while cursor < limit {
                let byte = bytes[cursor];
                if byte.is_ascii_whitespace()
                    || matches!(byte, b',' | b'}')
                    || (byte == b'/' && matches!(bytes.get(cursor + 1), Some(b'/' | b'*')))
                {
                    break;
                }
                cursor += 1;
            }
            if cursor == start {
                bail!("missing JSONC value");
            }
            Ok(cursor)
        }
        None => bail!("missing JSONC value"),
    }
}

fn find_matching_jsonc_container(
    raw: &str,
    open_index: usize,
    open: u8,
    close: u8,
) -> Option<usize> {
    let bytes = raw.as_bytes();
    if bytes.get(open_index) != Some(&open) {
        return None;
    }
    let mut stack = vec![close];
    let mut cursor = open_index + 1;
    let mut in_string = false;
    let mut escaped = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            cursor += 1;
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
                continue;
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < bytes.len()
                    && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/')
                {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(bytes.len());
                continue;
            }
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' if stack.last() == Some(&byte) => {
                stack.pop();
                if stack.is_empty() {
                    return Some(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn skip_jsonc_trivia(raw: &str, cursor: &mut usize, limit: usize) -> Result<()> {
    let bytes = raw.as_bytes();
    loop {
        while *cursor < limit && bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
        if *cursor + 1 < limit && bytes[*cursor] == b'/' && bytes[*cursor + 1] == b'/' {
            *cursor += 2;
            while *cursor < limit && bytes[*cursor] != b'\n' {
                *cursor += 1;
            }
        } else if *cursor + 1 < limit && bytes[*cursor] == b'/' && bytes[*cursor + 1] == b'*' {
            *cursor += 2;
            while *cursor + 1 < limit && !(bytes[*cursor] == b'*' && bytes[*cursor + 1] == b'/') {
                *cursor += 1;
            }
            if *cursor + 1 >= limit {
                bail!("unterminated JSONC block comment");
            }
            *cursor += 2;
        } else {
            return Ok(());
        }
    }
}

fn state_object_mut<'a>(
    state: &'a mut serde_json::Value,
    key: &str,
) -> Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    let state = state
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode managed state root is not an object"))?;
    let value = state
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode managed state {key} is not an object"))
}

fn state_bool(state: &serde_json::Value, key: &str) -> bool {
    state
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn legacy_rtrt_ollama_provider() -> serde_json::Value {
    serde_json::json!({
        "name": "Ollama",
        "npm": "@ai-sdk/openai-compatible",
        "options": {"baseURL": "http://localhost:11434/v1"},
        "models": {}
    })
}

fn opencode_ollama_provider_required() -> anyhow::Error {
    anyhow::anyhow!(
        "OpenCode TeamConfig requires Ollama models, but provider.ollama is not explicitly configured; operator must explicitly configure and approve provider.ollama before running rtrt setup (rtrt will not install, download, or add an npm adapter)"
    )
}

fn preflight_opencode_ollama_provider(
    config_path: &Path,
    state_path: &Path,
    plan: &OpenCodeTaskAgentPlan,
    state: &mut serde_json::Value,
    apply: bool,
) -> Result<()> {
    if plan.ollama_models.is_empty() {
        return Ok(());
    }
    let (raw, mut root) = read_opencode_config(config_path)?;
    let before = root.clone();
    let provider = root
        .get("provider")
        .and_then(serde_json::Value::as_object)
        .and_then(|providers| providers.get("ollama"));
    if state_bool(state, "ollama_created") && provider == Some(&legacy_rtrt_ollama_provider()) {
        if apply {
            if let Some(providers) = root
                .get_mut("provider")
                .and_then(serde_json::Value::as_object_mut)
            {
                providers.remove("ollama");
                if providers.is_empty() {
                    root.as_object_mut()
                        .expect("config root object")
                        .remove("provider");
                }
            }
            write_opencode_config(config_path, &raw, &before, &root)?;
            if let Some(state_object) = state.as_object_mut() {
                state_object.remove("ollama_created");
                state_object.remove("models_created");
            }
            if state_path.exists() {
                std::fs::remove_file(state_path)
                    .with_context(|| format!("remove {}", state_path.display()))?;
            }
        }
        bail!(
            "legacy RTRT-owned provider.ollama was removed safely; operator must explicitly configure and approve provider.ollama before rerunning rtrt setup (rtrt will not install, download, or add an npm adapter)"
        );
    }
    if provider.is_none() {
        return Err(opencode_ollama_provider_required());
    }
    Ok(())
}

fn require_opencode_ollama_models<'a>(
    root: &'a mut serde_json::Value,
    state: &mut serde_json::Value,
) -> Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    let providers = root
        .get_mut("provider")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("OpenCode config provider is not an object"))?;
    let ollama = providers
        .get_mut("ollama")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("OpenCode config provider.ollama is not an object"))?;
    if !ollama.contains_key("models") {
        ollama.insert("models".to_string(), serde_json::json!({}));
        state
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("OpenCode managed state root is not an object"))?
            .insert("models_created".to_string(), serde_json::Value::Bool(true));
    }
    ollama
        .get_mut("models")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("OpenCode config provider.ollama.models is not an object"))
}

fn opencode_ollama_models_mut(
    root: &mut serde_json::Value,
) -> Option<&mut serde_json::Map<String, serde_json::Value>> {
    root.get_mut("provider")?
        .get_mut("ollama")?
        .get_mut("models")?
        .as_object_mut()
}

fn sync_opencode_ollama_models(
    root: &mut serde_json::Value,
    desired: &BTreeSet<String>,
    state: &mut serde_json::Value,
) -> Result<()> {
    let obsolete: Vec<String> = state
        .get("models")
        .and_then(serde_json::Value::as_object)
        .map(|models| {
            models
                .keys()
                .filter(|model| !desired.contains(*model))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    for model in obsolete {
        restore_opencode_ollama_model(root, state, &model)?;
    }
    if desired.is_empty() {
        cleanup_opencode_created_config_objects(root, state);
        return Ok(());
    }

    for model in desired {
        let tracked = state
            .get("models")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|models| models.contains_key(model));
        if !tracked {
            let models = require_opencode_ollama_models(root, state)?;
            let entry_created = !models.contains_key(model);
            if entry_created {
                models.insert(model.clone(), serde_json::json!({}));
            }
            let entry = models
                .get(model)
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "OpenCode config provider.ollama.models.{model} is not an object"
                    )
                })?;
            let record = serde_json::json!({
                "entry_created": entry_created,
                "tool_call": optional_json_value(entry.get("tool_call"))
            });
            state_object_mut(state, "models")?.insert(model.clone(), record);
        }
        let models = require_opencode_ollama_models(root, state)?;
        let entry = models
            .get_mut(model)
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| {
                anyhow::anyhow!("OpenCode config provider.ollama.models.{model} is not an object")
            })?;
        entry.insert("tool_call".to_string(), serde_json::Value::Bool(true));
    }
    Ok(())
}

fn restore_opencode_ollama_model(
    root: &mut serde_json::Value,
    state: &mut serde_json::Value,
    model: &str,
) -> Result<()> {
    let record = state
        .get("models")
        .and_then(serde_json::Value::as_object)
        .and_then(|models| models.get(model))
        .cloned();
    let Some(record) = record else {
        return Ok(());
    };
    if let Some(models) = opencode_ollama_models_mut(root) {
        let mut remove_entry = false;
        if let Some(entry) = models
            .get_mut(model)
            .and_then(serde_json::Value::as_object_mut)
        {
            if entry.get("tool_call") == Some(&serde_json::Value::Bool(true))
                && let Some(prior) = record.get("tool_call")
            {
                restore_optional_json_value(entry, "tool_call", prior);
            }
            remove_entry = record
                .get("entry_created")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && entry.is_empty();
        }
        if remove_entry {
            models.remove(model);
        }
    }
    state_object_mut(state, "models")?.remove(model);
    Ok(())
}

fn restore_all_opencode_ollama_models(
    root: &mut serde_json::Value,
    state: &mut serde_json::Value,
) -> Result<()> {
    let models: Vec<String> = state
        .get("models")
        .and_then(serde_json::Value::as_object)
        .map(|models| models.keys().cloned().collect())
        .unwrap_or_default();
    for model in models {
        restore_opencode_ollama_model(root, state, &model)?;
    }
    Ok(())
}

fn restore_opencode_task_rule(
    task: &mut serde_json::Map<String, serde_json::Value>,
    state: &mut serde_json::Value,
    agent: &str,
) -> Result<()> {
    let prior = state
        .get("task_rules")
        .and_then(serde_json::Value::as_object)
        .and_then(|rules| rules.get(agent))
        .cloned();
    if task.get(agent).and_then(serde_json::Value::as_str) == Some("allow")
        && let Some(prior) = &prior
    {
        restore_optional_json_value(task, agent, prior);
    }
    state_object_mut(state, "task_rules")?.remove(agent);
    Ok(())
}

fn restore_all_opencode_task_permissions(
    root: &mut serde_json::Value,
    state: &mut serde_json::Value,
) -> Result<()> {
    let agents: Vec<String> = state
        .get("task_rules")
        .and_then(serde_json::Value::as_object)
        .map(|rules| rules.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(task) = root
        .get_mut("permission")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|permission| permission.get_mut("task"))
        .and_then(serde_json::Value::as_object_mut)
    {
        for agent in agents {
            restore_opencode_task_rule(task, state, &agent)?;
        }
    } else {
        state_object_mut(state, "task_rules")?.clear();
    }
    restore_config_baseline(root, state, "task_baseline", &["permission", "task"]);
    restore_config_baseline(root, state, "permission_baseline", &["permission"]);
    if let Some(state) = state.as_object_mut() {
        state.remove("task_rules");
    }
    Ok(())
}

fn restore_config_baseline(
    root: &mut serde_json::Value,
    state: &mut serde_json::Value,
    state_key: &str,
    path: &[&str],
) {
    let Some(record) = state.get(state_key).cloned() else {
        return;
    };
    let prior_present = record
        .get("present")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let prior = record.get("value").cloned();
    if path == ["permission", "task"] {
        if let Some(permission) = root
            .get_mut("permission")
            .and_then(serde_json::Value::as_object_mut)
        {
            let can_restore = match (permission.get("task"), prior.as_ref()) {
                (Some(value), Some(prior)) => value
                    .as_object()
                    .is_some_and(|task| task.len() == 1 && task.get("*") == Some(prior)),
                (Some(value), None) => value.as_object().is_some_and(serde_json::Map::is_empty),
                _ => false,
            };
            if can_restore {
                if prior_present {
                    permission.insert("task".to_string(), prior.unwrap_or(serde_json::Value::Null));
                } else {
                    permission.remove("task");
                }
            }
        }
    } else if path == ["permission"] {
        let can_restore = match (root.get("permission"), prior.as_ref()) {
            (Some(value), Some(prior)) => value.as_object().is_some_and(|permission| {
                permission.len() == 1 && permission.get("*") == Some(prior)
            }),
            (Some(value), None) => value.as_object().is_some_and(serde_json::Map::is_empty),
            _ => false,
        };
        if can_restore && let Some(root) = root.as_object_mut() {
            if prior_present {
                root.insert(
                    "permission".to_string(),
                    prior.unwrap_or(serde_json::Value::Null),
                );
            } else {
                root.remove("permission");
            }
        }
    }
    if let Some(state) = state.as_object_mut() {
        state.remove(state_key);
    }
}

fn sync_opencode_subagent_depth(
    root: &mut serde_json::Value,
    state: &mut serde_json::Value,
) -> Result<()> {
    let root = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("OpenCode config root is not a JSON object"))?;
    let needs_change = root
        .get("subagent_depth")
        .and_then(serde_json::Value::as_u64)
        .is_none_or(|depth| depth < 1);
    if needs_change {
        state
            .as_object_mut()
            .expect("validated state object")
            .entry("subagent_depth")
            .or_insert_with(|| optional_json_value(root.get("subagent_depth")));
        root.insert("subagent_depth".to_string(), serde_json::json!(1));
    }
    Ok(())
}

fn restore_opencode_subagent_depth(root: &mut serde_json::Value, state: &mut serde_json::Value) {
    let Some(record) = state.get("subagent_depth").cloned() else {
        return;
    };
    if let Some(root) = root.as_object_mut()
        && root.get("subagent_depth") == Some(&serde_json::json!(1))
    {
        restore_optional_json_value(root, "subagent_depth", &record);
    }
    if let Some(state) = state.as_object_mut() {
        state.remove("subagent_depth");
    }
}

fn cleanup_opencode_created_config_objects(
    root: &mut serde_json::Value,
    state: &mut serde_json::Value,
) {
    let models_empty = root
        .get("provider")
        .and_then(|provider| provider.get("ollama"))
        .and_then(|ollama| ollama.get("models"))
        .and_then(serde_json::Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    if state_bool(state, "models_created") && models_empty {
        if let Some(ollama) = root
            .get_mut("provider")
            .and_then(|provider| provider.get_mut("ollama"))
            .and_then(serde_json::Value::as_object_mut)
        {
            ollama.remove("models");
        }
        state
            .as_object_mut()
            .expect("state object")
            .remove("models_created");
    }
    let ollama_owned_and_untouched = root
        .get("provider")
        .and_then(|provider| provider.get("ollama"))
        .is_some_and(|ollama| {
            ollama == &legacy_rtrt_ollama_provider()
                || ollama.as_object().is_some_and(serde_json::Map::is_empty)
        });
    if state_bool(state, "ollama_created") && ollama_owned_and_untouched {
        if let Some(provider) = root
            .get_mut("provider")
            .and_then(serde_json::Value::as_object_mut)
        {
            provider.remove("ollama");
        }
        state
            .as_object_mut()
            .expect("state object")
            .remove("ollama_created");
    }
    let provider_empty = root
        .get("provider")
        .and_then(serde_json::Value::as_object)
        .is_some_and(serde_json::Map::is_empty);
    if state_bool(state, "provider_created") && provider_empty {
        if let Some(root) = root.as_object_mut() {
            root.remove("provider");
        }
        state
            .as_object_mut()
            .expect("state object")
            .remove("provider_created");
    }
}

fn install_opencode_agents_rules(apply: bool) -> Result<()> {
    let path = expand_home(OPENCODE_RULES_REL)?;
    install_opencode_agents_rules_at(&path, apply)
}

fn install_opencode_agents_rules_at(path: &Path, apply: bool) -> Result<()> {
    let terse_block = terse_rules_block();
    let workspace_block = opencode_workspace_rules_block();
    if !apply {
        println!("[dry-run] OpenCode rules target: {}", path.display());
        println!("[dry-run] OpenCode rules blocks:\n{terse_block}\n{workspace_block}");
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    ensure_managed_block_recognized(&existing, &terse_block, TERSE_BLOCK_BEGIN, TERSE_BLOCK_END)?;
    ensure_managed_block_recognized(
        &existing,
        &workspace_block,
        OPENCODE_WORKSPACE_BLOCK_BEGIN,
        OPENCODE_WORKSPACE_BLOCK_END,
    )?;
    let mut rendered = existing.clone();
    if !rendered.contains(&terse_block) {
        append_managed_block(&mut rendered, &terse_block);
    }
    if !rendered.contains(&workspace_block) {
        append_managed_block(&mut rendered, &workspace_block);
    }
    if path.exists() && existing == rendered {
        println!(
            "{}: managed rtrt OpenCode rules already present",
            path.display()
        );
        return Ok(());
    }
    if path.exists() {
        backup_if_needed(path)?;
    }
    std::fs::write(path, rendered).with_context(|| format!("write {}", path.display()))?;
    println!("wrote managed rtrt OpenCode rules to {}", path.display());
    Ok(())
}

fn append_managed_block(existing: &mut String, block: &str) {
    if !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(block);
}

fn ensure_managed_block_recognized(
    existing: &str,
    block: &str,
    begin: &str,
    end: &str,
) -> Result<()> {
    let unmanaged = existing.replace(block, "");
    if unmanaged
        .lines()
        .any(|line| matches!(line.trim(), marker if marker == begin || marker == end))
    {
        bail!(
            "OpenCode rules contain a modified or malformed managed block; refusing to overwrite it"
        );
    }
    Ok(())
}

fn remove_exact_managed_block(existing: &str, block: &str) -> (String, bool) {
    let mut rendered = existing.to_string();
    let mut removed = false;
    while let Some(block_start) = rendered.find(block) {
        let start = if block_start > 0 && rendered.as_bytes()[block_start - 1] == b'\n' {
            block_start - 1
        } else {
            block_start
        };
        let end = block_start + block.len();
        rendered.replace_range(start..end, "");
        removed = true;
    }
    (rendered, removed)
}

fn install_opencode_provenance_plugin(apply: bool) -> Result<()> {
    let path = expand_home(OPENCODE_PROVENANCE_PLUGIN_REL)?;
    install_opencode_provenance_plugin_at(&path, apply)
}

fn managed_opencode_provenance_plugin() -> String {
    debug_assert!(is_whole_file_managed_provenance_plugin(
        OPENCODE_PROVENANCE_PLUGIN
    ));
    OPENCODE_PROVENANCE_PLUGIN.to_string()
}

fn opencode_provenance_state_path(plugin_path: &Path) -> Result<PathBuf> {
    let parent = plugin_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{}: plugin has no parent", plugin_path.display()))?;
    Ok(parent.join(OPENCODE_PROVENANCE_STATE_FILE))
}

fn exact_provenance_plugin_path(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("{}: plugin path is not valid UTF-8", path.display()))
}

fn is_whole_file_managed_provenance_plugin(content: &str) -> bool {
    if content.contains('\0')
        || !content.starts_with(&format!("{OPENCODE_PROVENANCE_PLUGIN_BEGIN}\n"))
        || content.matches(OPENCODE_PROVENANCE_PLUGIN_BEGIN).count() != 1
        || content.matches(OPENCODE_PROVENANCE_PLUGIN_END).count() != 1
    {
        return false;
    }
    let without_trailing_newline = content.strip_suffix('\n').unwrap_or(content);
    if !without_trailing_newline.ends_with(OPENCODE_PROVENANCE_PLUGIN_END)
        || !without_trailing_newline
            [..without_trailing_newline.len() - OPENCODE_PROVENANCE_PLUGIN_END.len()]
            .ends_with('\n')
    {
        return false;
    }
    [
        "export const RtrtProvenance",
        "\"chat.message\"",
        "\"tool.execute.before\"",
        "\"shell.env\"",
        "\"tool.execute.after\"",
    ]
    .iter()
    .all(|anchor| content.contains(anchor))
}

fn load_opencode_provenance_state(state_path: &Path, plugin_path: &Path) -> Result<Option<String>> {
    let Some(metadata) = path_metadata(state_path)? else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{}: provenance ownership state is not a real file",
            state_path.display()
        );
    }
    let raw = std::fs::read_to_string(state_path)
        .with_context(|| format!("read {}", state_path.display()))?;
    let state: serde_json::Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "{}: invalid provenance ownership state JSON",
            state_path.display()
        )
    })?;
    let Some(root) = state.as_object() else {
        bail!(
            "{}: invalid provenance ownership state root",
            state_path.display()
        );
    };
    let installed_content = root.get("content").and_then(serde_json::Value::as_str);
    if root.len() != 4
        || root.get("owner").and_then(serde_json::Value::as_str)
            != Some(OPENCODE_PROVENANCE_STATE_OWNER)
        || root.get("version").and_then(serde_json::Value::as_u64)
            != Some(OPENCODE_PROVENANCE_STATE_VERSION)
        || root.get("path").and_then(serde_json::Value::as_str)
            != Some(exact_provenance_plugin_path(plugin_path)?)
        || !installed_content.is_some_and(is_whole_file_managed_provenance_plugin)
    {
        bail!(
            "{}: invalid provenance ownership state",
            state_path.display()
        );
    }
    Ok(installed_content.map(str::to_owned))
}

fn write_opencode_provenance_state(
    state_path: &Path,
    plugin_path: &Path,
    installed_content: &str,
) -> Result<()> {
    debug_assert!(is_whole_file_managed_provenance_plugin(installed_content));
    let state = serde_json::json!({
        "owner": OPENCODE_PROVENANCE_STATE_OWNER,
        "version": OPENCODE_PROVENANCE_STATE_VERSION,
        "path": exact_provenance_plugin_path(plugin_path)?,
        "content": installed_content,
    });
    let rendered = serde_json::to_string_pretty(&state)?;
    write_private_file_atomic_same_dir(state_path, rendered.as_bytes())
        .with_context(|| format!("write {}", state_path.display()))
}

fn read_real_file(path: &Path, label: &str) -> Result<Option<String>> {
    let Some(metadata) = path_metadata(path)? else {
        return Ok(None);
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{}: {label} is not a real file", path.display());
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))
        .map(Some)
}

fn install_opencode_provenance_plugin_at(path: &Path, apply: bool) -> Result<()> {
    let rendered = managed_opencode_provenance_plugin();
    let state_path = opencode_provenance_state_path(path)?;
    let state_content = load_opencode_provenance_state(&state_path, path)?;
    let existing = read_real_file(path, "provenance plugin")?;
    if !apply {
        println!("[dry-run] OpenCode provenance plugin: {}", path.display());
        return Ok(());
    }
    if let Some(existing) = existing {
        if state_content
            .as_ref()
            .is_some_and(|installed| installed != &existing)
        {
            bail!(
                "{}: provenance plugin differs from private ownership state; refusing to overwrite it",
                path.display()
            );
        }
        if existing == rendered {
            write_opencode_provenance_state(&state_path, path, &rendered)?;
            println!(
                "{}: managed OpenCode provenance plugin already present",
                path.display()
            );
            return Ok(());
        }
        if existing != OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1
            && existing != OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2
            && state_content.as_deref() != Some(existing.as_str())
            && !(state_content.is_none() && is_whole_file_managed_provenance_plugin(&existing))
        {
            bail!(
                "{}: refusing to overwrite unrecognized pre-existing plugin content",
                path.display()
            );
        }
        backup_if_needed(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    write_private_file_atomic_same_dir(path, rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    write_opencode_provenance_state(&state_path, path, &rendered)?;
    println!(
        "wrote OpenCode provenance plugin to {} (restart OpenCode to load it)",
        path.display()
    );
    Ok(())
}

fn remove_opencode_provenance_plugin(apply: bool) -> Result<()> {
    let path = expand_home(OPENCODE_PROVENANCE_PLUGIN_REL)?;
    remove_opencode_provenance_plugin_at(&path, apply)
}

fn remove_opencode_provenance_plugin_at(path: &Path, apply: bool) -> Result<()> {
    let state_path = opencode_provenance_state_path(path)?;
    let state_content = load_opencode_provenance_state(&state_path, path)?;
    let existing = read_real_file(path, "provenance plugin")?;
    if !apply {
        println!("[dry-run] would remove {}", path.display());
        return Ok(());
    }
    let Some(raw) = existing else {
        return Ok(());
    };
    let managed = managed_opencode_provenance_plugin();
    let recognized = if let Some(installed) = state_content.as_deref() {
        raw == installed
    } else {
        raw == managed
            || raw == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1
            || raw == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2
            || is_whole_file_managed_provenance_plugin(&raw)
    };
    if !recognized {
        println!(
            "{}: plugin content was modified or is not rtrt-managed; preserving it",
            path.display()
        );
        return Ok(());
    }

    let backup = backup_path(path);
    let backup_content = if let Some(content) = read_real_file(&backup, "provenance backup")? {
        if content == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1
            || content == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2
            || is_whole_file_managed_provenance_plugin(&content)
        {
            String::new()
        } else {
            content
        }
    } else {
        String::new()
    };
    if backup_content.is_empty() {
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    } else {
        write_private_file_atomic_same_dir(path, backup_content.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    if path_metadata(&backup)?.is_some() {
        std::fs::remove_file(&backup).with_context(|| format!("remove {}", backup.display()))?;
    }
    if path_metadata(&state_path)?.is_some() {
        std::fs::remove_file(&state_path)
            .with_context(|| format!("remove {}", state_path.display()))?;
    }
    println!(
        "removed managed OpenCode provenance plugin from {}",
        path.display()
    );
    Ok(())
}

/// Exact-file migration helper retained for a future explicitly scoped cleanup.
#[allow(dead_code)]
fn remove_opencode_provenance_bridge_at(settings: &Path, apply: bool) -> Result<()> {
    if !settings.exists() {
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would remove the OpenCode provenance bridge from {}",
            settings.display()
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(settings)
        .with_context(|| format!("read {}", settings.display()))?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{}: not valid JSON", settings.display()))?;
    let mut touched = false;
    if let Some(entries) = root
        .get_mut("hooks")
        .and_then(|value| value.get_mut("SessionStart"))
        .and_then(|value| value.as_array_mut())
    {
        touched = remove_provenance_handlers(entries, "opencode");
    }
    if touched {
        backup_if_needed(settings)?;
        std::fs::write(settings, serde_json::to_string_pretty(&root)?)
            .with_context(|| format!("write {}", settings.display()))?;
        println!(
            "removed OpenCode provenance bridge from {}",
            settings.display()
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
    remove_managed_block_from_text(existing, TERSE_BLOCK_BEGIN, TERSE_BLOCK_END)
}

fn remove_managed_block_from_text(existing: &str, begin: &str, end: &str) -> (String, bool) {
    let mut out = String::with_capacity(existing.len());
    let mut skipping = false;
    let mut removed = false;
    for line in existing.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\r', '\n']).trim();
        if marker == begin {
            skipping = true;
            removed = true;
            continue;
        }
        if skipping {
            if marker == end {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
    }
    (out, removed)
}

fn remove_opencode_agents_rules(apply: bool) -> Result<()> {
    let path = expand_home(OPENCODE_RULES_REL)?;
    remove_opencode_agents_rules_at(&path, apply)
}

fn remove_opencode_agents_rules_at(path: &Path, apply: bool) -> Result<()> {
    if !path.exists() {
        println!("{}: not present", path.display());
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would remove managed rtrt OpenCode rules blocks from {}",
            path.display()
        );
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (without_terse, terse_removed) = remove_exact_managed_block(&raw, &terse_rules_block());
    let (rendered, workspace_removed) =
        remove_exact_managed_block(&without_terse, &opencode_workspace_rules_block());
    if terse_removed || workspace_removed {
        backup_if_needed(path)?;
        std::fs::write(path, rendered).with_context(|| format!("write {}", path.display()))?;
        println!(
            "removed managed rtrt OpenCode rules from {}",
            path.display()
        );
    } else {
        println!(
            "{}: managed rtrt OpenCode rules not present",
            path.display()
        );
    }
    Ok(())
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
                if remove_provenance_handlers(arr, "claude") {
                    touched = true;
                }
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
    remove_opencode_agents_rules(apply)?;
    Ok(())
}

/// Pick the `rtrt` command to embed in the hook line. Prefers the binary
/// next to the running CLI; falls back to the bare `rtrt` symbol so
/// `PATH` lookup still works when the sibling binary is unavailable.
fn locate_rtrt_binary() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(candidate) = sibling_rtrt_binary(&exe)
        && candidate.exists()
    {
        return candidate.to_string_lossy().into_owned();
    }
    "rtrt".to_string()
}

fn sibling_rtrt_binary(current_exe: &Path) -> Option<PathBuf> {
    let name = if current_exe
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        "rtrt.exe"
    } else {
        "rtrt"
    };
    current_exe.parent().map(|parent| parent.join(name))
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
            // Symmetric with setup: both plugin and non-plugin setup paths
            // install the statusLine, so both uninstall paths remove it.
            remove_claude_statusline(apply)?;
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
        AgentKind::Opencode => uninstall_opencode(apply),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenCodeUninstallSurface {
    Sandbox,
    TaskAgents,
    Rules,
    ProvenancePlugin,
    TuiStatusline,
    McpConfig,
}

impl OpenCodeUninstallSurface {
    fn label(self) -> &'static str {
        match self {
            Self::Sandbox => "shell sandbox",
            Self::TaskAgents => "Task agents",
            Self::Rules => "rules",
            Self::ProvenancePlugin => "provenance plugin",
            Self::TuiStatusline => "TUI statusline",
            Self::McpConfig => "MCP config",
        }
    }
}

fn run_opencode_uninstall_steps(
    mut run: impl FnMut(OpenCodeUninstallSurface) -> Result<()>,
) -> Result<()> {
    let mut errors = Vec::new();
    for surface in [
        OpenCodeUninstallSurface::TaskAgents,
        OpenCodeUninstallSurface::Rules,
        // Unregister before deleting the file so an interrupted uninstall
        // never leaves OpenCode pointing at a missing RTRT plugin.
        OpenCodeUninstallSurface::McpConfig,
        OpenCodeUninstallSurface::ProvenancePlugin,
        OpenCodeUninstallSurface::TuiStatusline,
        // Restore prior shell only after execution-agent removal succeeded.
        OpenCodeUninstallSurface::Sandbox,
    ] {
        if surface == OpenCodeUninstallSurface::Sandbox && !errors.is_empty() {
            errors.push("shell sandbox: retained because uninstall previously failed".to_string());
            continue;
        }
        if let Err(error) = run(surface) {
            errors.push(format!("{}: {error}", surface.label()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "OpenCode uninstall encountered {} error(s): {}",
            errors.len(),
            errors.join("; ")
        )
    }
}

fn uninstall_opencode(apply: bool) -> Result<()> {
    let agents_root = expand_home(OPENCODE_AGENTS_ROOT_REL)?;
    let fallback_config = resolve_opencode_config_path()?;
    let state_path = agents_root.join(OPENCODE_AGENT_STATE_FILE);
    let (config_path, mut ownership_error) = match load_opencode_agent_state(&state_path)
        .and_then(|state| preferred_opencode_config_path(&state, &fallback_config))
    {
        Ok(path) => (path, None),
        Err(error) => (fallback_config, Some(error.to_string())),
    };
    run_opencode_uninstall_steps(|surface| match surface {
        OpenCodeUninstallSurface::Sandbox => disable_opencode_sandbox_global(apply),
        OpenCodeUninstallSurface::TaskAgents => {
            if let Some(error) = ownership_error.take() {
                bail!("unsafe ownership state: {error}");
            }
            remove_opencode_task_agents_at(&agents_root, &config_path, apply)
        }
        OpenCodeUninstallSurface::Rules => remove_opencode_agents_rules(apply),
        OpenCodeUninstallSurface::ProvenancePlugin => remove_opencode_provenance_plugin(apply),
        OpenCodeUninstallSurface::TuiStatusline => remove_opencode_tui_statusline(apply),
        OpenCodeUninstallSurface::McpConfig => drop_opencode_jsonc_at(&config_path, apply),
    })
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
    if let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut())
        && servers.remove("rtrt").is_some()
    {
        let rendered = serde_json::to_string_pretty(&root)?;
        std::fs::write(&path, rendered).with_context(|| format!("write {}", path.display()))?;
        println!("dropped mcpServers.rtrt from {}", path.display());
        return Ok(());
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
    let metadata = path_metadata(path)?
        .ok_or_else(|| anyhow::anyhow!("{}: backup source is missing", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "{}: refusing symlink or non-file backup source",
            path.display()
        );
    }
    let bak = backup_path(path);
    if let Some(metadata) = path_metadata(&bak)? {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "{}: refusing symlink or non-file backup destination",
                bak.display()
            );
        }
        return Ok(());
    }
    std::fs::copy(path, &bak).with_context(|| format!("backup {}", bak.display()))?;
    set_private_file_mode(&bak)?;
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension({
        let mut e = path
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !e.is_empty() {
            e.push('.');
        }
        e.push_str("bak");
        e
    })
}

fn path_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn reject_symlink(path: &Path, label: &str) -> Result<()> {
    if path_metadata(path)?.is_some_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("{}: refusing symlink {label} destination", path.display());
    }
    Ok(())
}

fn write_private_file_atomic_same_dir(path: &Path, contents: &[u8]) -> Result<()> {
    reject_symlink(path, "atomic file")?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{}: file has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{}: file has no name", path.display()))?
        .to_string_lossy();
    let mut temporary = None;
    for sequence in 0..100u8 {
        let candidate = parent.join(format!(
            ".{file_name}.rtrt-{}-{sequence}.tmp",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()));
            }
        }
    }
    let (temporary_path, mut file) = temporary.ok_or_else(|| {
        anyhow::anyhow!(
            "{}: cannot allocate same-directory atomic temporary file",
            path.display()
        )
    })?;
    let result = (|| -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", temporary_path.display()))?;
        }
        file.write_all(contents)
            .with_context(|| format!("write {}", temporary_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary_path.display()))?;
        drop(file);
        std::fs::rename(&temporary_path, path).with_context(|| {
            format!(
                "atomically replace {} from {}",
                path.display(),
                temporary_path.display()
            )
        })?;
        set_private_file_mode(path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("sync directory {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

// ---------------------------------------------------------------------------
// Shared health checks — used by `rtrt project status`/`health` and
// `rtrt doctor` so the Claude Code integration / memory-store probes live in
// exactly one place. Every check reads real local state; nothing here is
// fabricated.
// ---------------------------------------------------------------------------

const CLAUDE_HOOK_NEEDLE: &str = "rtrt hook";
const CLAUDE_STATUSLINE_NEEDLE: &str = "statusline --rich";

/// Pass/warn/fail state for a single health-check row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Pass,
    Warn,
    Fail,
}

impl CheckState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

pub struct ClaudeSettingsStatus {
    pub hooks_state: CheckState,
    pub hooks_detail: String,
    pub statusline_state: CheckState,
    pub statusline_detail: String,
}

/// Checks `~/.claude/settings.json` for rtrt-owned hook entries and the rich
/// statusLine command. `health` escalates a read/parse failure from WARN to
/// FAIL (used by `rtrt project health`); a missing file is always WARN since
/// the Claude Code integration is opt-in.
pub fn claude_settings_status(health: bool) -> ClaudeSettingsStatus {
    let missing = ClaudeSettingsStatus {
        hooks_state: CheckState::Warn,
        hooks_detail: "settings file missing".into(),
        statusline_state: CheckState::Warn,
        statusline_detail: "settings file missing".into(),
    };
    let Some(settings_path) = dirs_home()
        .ok()
        .map(|home| home.join(".claude/settings.json"))
    else {
        return ClaudeSettingsStatus {
            hooks_detail: "home directory unavailable".into(),
            statusline_detail: "home directory unavailable".into(),
            ..missing
        };
    };
    if !settings_path.exists() {
        return missing;
    }
    let raw = match std::fs::read_to_string(&settings_path) {
        Ok(raw) => raw,
        Err(err) => {
            let state = if health {
                CheckState::Fail
            } else {
                CheckState::Warn
            };
            return ClaudeSettingsStatus {
                hooks_state: state,
                hooks_detail: format!("read failed: {err}"),
                statusline_state: state,
                statusline_detail: format!("read failed: {err}"),
            };
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            let state = if health {
                CheckState::Fail
            } else {
                CheckState::Warn
            };
            return ClaudeSettingsStatus {
                hooks_state: state,
                hooks_detail: format!("invalid JSON: {err}"),
                statusline_state: state,
                statusline_detail: format!("invalid JSON: {err}"),
            };
        }
    };
    let hooks_present = parsed
        .get("hooks")
        .is_some_and(|hooks| json_contains_text(hooks, CLAUDE_HOOK_NEEDLE));
    let statusline_present = parsed
        .get("statusLine")
        .is_some_and(|statusline| json_contains_text(statusline, CLAUDE_STATUSLINE_NEEDLE));
    ClaudeSettingsStatus {
        hooks_state: if hooks_present {
            CheckState::Pass
        } else {
            CheckState::Warn
        },
        hooks_detail: if hooks_present {
            "rtrt hook entries found".into()
        } else {
            "rtrt hook entries missing".into()
        },
        statusline_state: if statusline_present {
            CheckState::Pass
        } else {
            CheckState::Warn
        },
        statusline_detail: if statusline_present {
            "rtrt rich statusLine found".into()
        } else {
            "rtrt rich statusLine missing".into()
        },
    }
}

fn json_contains_text(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(s) => s.contains(needle),
        serde_json::Value::Array(items) => {
            items.iter().any(|item| json_contains_text(item, needle))
        }
        serde_json::Value::Object(map) => map.values().any(|item| json_contains_text(item, needle)),
        _ => false,
    }
}

/// Checks whether `~/.claude.json` registers the `rtrt` MCP server
/// (`mcpServers.rtrt`) — the entry `rtrt setup --agent claude --apply` writes.
pub fn claude_mcp_registered_status() -> (CheckState, String) {
    let Some(path) = dirs_home().ok().map(|home| home.join(".claude.json")) else {
        return (CheckState::Warn, "home directory unavailable".into());
    };
    if !path.exists() {
        return (CheckState::Warn, format!("missing {}", path.display()));
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => return (CheckState::Warn, format!("read failed: {err}")),
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => return (CheckState::Warn, format!("invalid JSON: {err}")),
    };
    let registered = parsed
        .get("mcpServers")
        .and_then(|servers| servers.get("rtrt"))
        .is_some();
    if registered {
        (
            CheckState::Pass,
            format!("rtrt registered in {}", path.display()),
        )
    } else {
        (
            CheckState::Warn,
            format!("rtrt not registered in {}", path.display()),
        )
    }
}

/// Opens the memory store read-only and confirms the `memories` table
/// answers a query, reporting the row count on success. `health` escalates
/// an existing-but-broken store from WARN to FAIL; a store that simply
/// hasn't been created yet is always WARN.
pub fn memory_reachable_status(health: bool) -> (CheckState, String) {
    let identity = match std::env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| rtrt_core::ProjectIdentity::derive(cwd).map_err(anyhow::Error::from))
    {
        Ok(identity) => identity,
        Err(err) => {
            return (
                CheckState::Fail,
                format!("project identity unavailable: {err}"),
            );
        }
    };
    let path = match rtrt_core::project_memory_db_path(&identity) {
        Ok(path) => path,
        Err(err) => {
            return (
                CheckState::Fail,
                format!("project store unavailable: {err}"),
            );
        }
    };
    if !path.exists() {
        return (CheckState::Warn, format!("missing {}", path.display()));
    }
    match rtrt_memory::MemoryStore::open_project(&identity)
        .and_then(|store| store.count_by_project(identity.slug()))
    {
        Ok(count) => (
            CheckState::Pass,
            format!("reachable {} ({count} rows)", path.display()),
        ),
        Err(err) => (
            if health {
                CheckState::Fail
            } else {
                CheckState::Warn
            },
            format!("unreachable {}: {err}", path.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_member(
        name: &str,
        host_agent: &str,
        model: Option<&str>,
        roles: &[&str],
        fallback: &[&str],
    ) -> rtrt_core::TeamMember {
        rtrt_core::TeamMember {
            model: model.map(str::to_string),
            roles: roles.iter().map(|role| (*role).to_string()).collect(),
            host_agent: Some(host_agent.to_string()),
            fallback: fallback
                .iter()
                .map(|fallback| (*fallback).to_string())
                .collect(),
            ..rtrt_core::TeamMember::new(name, "opencode", rtrt_core::TeamMode::Cli)
        }
    }

    fn opencode_cloud_team() -> rtrt_core::TeamConfig {
        let mut shell =
            rtrt_core::TeamMember::new("claude-plan", "claude", rtrt_core::TeamMode::Cli);
        shell.model = Some("opus".to_string());
        shell.roles = vec!["plan".to_string()];
        shell.delegation = rtrt_core::Delegation::Shell;
        shell.host_agent = Some("claude-opus".to_string());
        shell.allow_impl = false;
        shell
            .flags
            .insert("output-format".to_string(), "json".to_string());
        shell
            .flags
            .insert("permission-mode".to_string(), "plan".to_string());
        shell.flags.insert(
            CLAUDE_PERMISSION_PROMPT_FLAG.to_string(),
            CLAUDE_PERMISSION_PROMPT_TOOL.to_string(),
        );

        rtrt_core::TeamConfig {
            enabled: true,
            manager_provider: "ollama".to_string(),
            manager_model: "glm-5.2:cloud".to_string(),
            leader_order: vec!["codex-sol".to_string(), "kimi-k3".to_string()],
            members: vec![
                native_member(
                    "glm",
                    "glm",
                    Some("opencode-go/glm-5.2"),
                    &["simple", "mechanical"],
                    &["glm-cloud"],
                ),
                native_member(
                    "glm-cloud",
                    "glm-cloud",
                    Some("ollama/glm-5.2:cloud"),
                    &["simple", "overflow"],
                    &["kimi"],
                ),
                native_member(
                    "kimi",
                    "kimi",
                    Some("opencode-go/kimi-k2.7-code"),
                    &["simple", "single-file"],
                    &["kimi-cloud"],
                ),
                native_member(
                    "kimi-cloud",
                    "kimi-cloud",
                    Some("ollama/kimi-k2.7-code:cloud"),
                    &["routine", "tests"],
                    &["kimi-k3"],
                ),
                native_member(
                    "kimi-k3",
                    "kimi-k3",
                    Some("opencode-go/kimi-k3"),
                    &["hard-implementation", "multifile"],
                    &["kimi-k3-cloud"],
                ),
                native_member(
                    "kimi-k3-cloud",
                    "kimi-k3-cloud",
                    Some("ollama/kimi-k3:cloud"),
                    &["hard-implementation", "overflow"],
                    &["codex-sol"],
                ),
                native_member(
                    "codex-sol",
                    "codex-sol-worker",
                    Some("openai/gpt-5.6-sol"),
                    &["hard-implementation", "debugging"],
                    &["claude-plan"],
                ),
                native_member(
                    "codex-luna",
                    "codex-luna",
                    Some("openai/gpt-5.6-luna"),
                    &["routine", "tests"],
                    &["codex-sol"],
                ),
                shell,
            ],
            policy: rtrt_core::TeamPolicy {
                worker_summary_max_lines: 4,
                ..rtrt_core::TeamPolicy::default()
            },
            ..rtrt_core::TeamConfig::default()
        }
    }

    #[test]
    fn claude_statusline_entry_uses_rich_command() {
        let entry = claude_statusline_entry("rtrt");

        assert_eq!(entry.get("type").and_then(|v| v.as_str()), Some("command"));
        assert_eq!(
            entry.get("command").and_then(|v| v.as_str()),
            Some("rtrt statusline --rich")
        );
    }

    #[test]
    fn strip_rtrt_statusline_removes_rtrt_entry() {
        let mut root = serde_json::json!({
            "statusLine": claude_statusline_entry("/home/u/.local/bin/rtrt"),
            "other": true
        });

        assert!(strip_rtrt_statusline(&mut root));
        assert!(root.get("statusLine").is_none());
        assert_eq!(root.get("other"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn strip_rtrt_statusline_preserves_user_entry() {
        let mut root = serde_json::json!({
            "statusLine": { "type": "command", "command": "my-own-statusline" }
        });

        assert!(!strip_rtrt_statusline(&mut root));
        assert!(root.get("statusLine").is_some());
    }

    #[test]
    fn opencode_entry_has_local_type_command_array_and_enabled() {
        let memory_path = Some(PathBuf::from("/home/u/.rtrt/memory.sqlite"));
        let entry = build_opencode_entry("/usr/local/bin/rtrt-mcp", &memory_path);

        assert_eq!(entry.get("type").and_then(|v| v.as_str()), Some("local"));
        assert_eq!(entry.get("enabled").and_then(|v| v.as_bool()), Some(true));
        let command: Vec<&str> = entry
            .get("command")
            .and_then(|v| v.as_array())
            .expect("command must be an array")
            .iter()
            .map(|v| v.as_str().expect("command items must be strings"))
            .collect();
        assert_eq!(
            command,
            vec![
                "/usr/local/bin/rtrt-mcp",
                "--memory",
                "/home/u/.rtrt/memory.sqlite",
            ]
        );
    }

    #[test]
    fn opencode_task_plan_maps_cloud_pairs_codex_and_excludes_shell_lanes() {
        let team = rtrt_core::TeamConfig::preset(rtrt_core::RosterPreset::OpencodeLead);
        let plan = build_opencode_task_agent_plan(&team).unwrap();

        assert_eq!(
            plan.files.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "codex-luna",
                "codex-sol-worker",
                "glm",
                "glm-cloud",
                "kimi",
                "kimi-cloud",
                "kimi-k3",
                "kimi-k3-cloud",
                "rtrt-manager",
            ]
        );
        assert_eq!(
            plan.ollama_models,
            BTreeSet::from([
                "glm-5.2:cloud".to_string(),
                "kimi-k2.7-code:cloud".to_string(),
                "kimi-k3:cloud".to_string(),
            ])
        );
        assert_eq!(plan.excluded_shell.len(), 2);
        assert_eq!(plan.excluded_shell[0].0, "opus");
        assert_eq!(plan.excluded_shell[1].0, "sonnet");
        assert!(!plan.files.contains_key("claude-opus"));
        assert!(!plan.files.contains_key("claude-sonnet"));

        let manager = &plan.files[OPENCODE_MANAGER_AGENT];
        assert!(manager.contains("mode: primary"));
        assert!(manager.contains("model: \"openai/gpt-5.6-sol\""));
        assert!(manager.contains(
            "lane=\"glm-cloud\" host=\"glm-cloud\" roles=[\"simple\",\"mechanical\",\"boilerplate\",\"bulk-edit\"]"
        ));
        assert!(manager.contains("sibling_native_host=\"glm\""));
        assert!(manager.contains("fallback_routes=[\"kimi-cloud => Task(kimi-cloud)\"]"));
        assert!(manager.contains(
            "lane=\"codex-sol\" host=\"codex-sol-worker\" roles=[\"hard-implementation\",\"debugging\",\"systems\",\"architecture-aware\"] sibling_native_host=null fallback_routes=[\"sonnet => claude -p --model sonnet --output-format json --permission-mode acceptEdits --permission-prompt-tool mcp__rtrt__permission_prompt '<PROMPT>'\"]"
        ));
        assert!(manager.contains("\"codex-sol-worker\": allow"));
        assert!(manager.contains("Effective tiers (configured executable order)"));
        assert!(
            manager.contains("- \"hard\": [\"codex-sol-worker\",\"kimi-k3-cloud\",\"kimi-k3\"]")
        );
        assert!(manager.contains("- leader_order=[\"codex-sol\",\"sonnet\",\"kimi-k3-cloud\"]"));
        assert!(manager.contains(
            "- \"plan\": [\"claude -p --model opus --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt '<PROMPT>'\"]"
        ));
        assert!(manager.contains(
            "- \"review\": [\"claude -p --model sonnet --output-format json --permission-mode acceptEdits --permission-prompt-tool mcp__rtrt__permission_prompt '<PROMPT>'\"]"
        ));
        assert!(manager.contains("default_tier=\"hard\"; explore_tier=\"explore\"; review_tier=\"review\"; design_only_tiers=[\"plan\"]"));
        assert!(manager.contains("max_retries=2"));
        assert!(manager.contains("prefer_sibling_on_quota=true"));
        assert!(manager.contains("balance=\"room\""));
        assert!(manager.contains("Classify and route work only from the rendered configuration"));
        assert!(manager.contains("Source code does not impose model-name shortcuts"));
        assert!(manager.contains(
            "Select a configured plan or review lane only when its configured roles and tier fit"
        ));
        assert!(manager.contains(
            "Preserve configured tier/lane order whenever no trustworthy room signal exists"
        ));
        assert!(manager.contains(
            "lane=\"sonnet\" model=\"sonnet\" roles=[\"review\",\"consistency\"] flags={\"output-format\":\"json\",\"permission-mode\":\"acceptEdits\",\"permission-prompt-tool\":\"mcp__rtrt__permission_prompt\"} delegation=CLI argv_template=\"claude -p --model sonnet --output-format json --permission-mode acceptEdits --permission-prompt-tool mcp__rtrt__permission_prompt '<PROMPT>'\""
        ));
        assert!(manager.contains("may not add `--allowed-tools`"));
        assert!(manager.contains("parse its JSON result"));
        assert!(manager.contains("Preserve the original task prompt semantically"));
        assert!(manager.contains(
            "The installed RTRT OpenCode plugin recognizes exactly three native-child provider limits: structured `429` with `rate_limit`, structured `529` with `capacity`, and provider transport `weekly_usage_limit` only when `session.next.retried` contains the exact machine code `weekly_usage_limit` or exact phrase `weekly usage limit` / `weekly usage cap`"
        ));
        assert!(manager.contains(
            "When a native Task returns interrupted/aborted from any of those circuit breakers, mark the exact selected lane unavailable for the current session, consume zero same-lane retries"
        ));
        assert!(manager.contains("immediately follow the configured sibling/fallback route"));
        assert!(manager.contains("`sibling_native_host` first when present and `prefer_sibling_on_quota=true`, then `fallback_routes`"));
        assert!(manager.contains(
            "A recognized weekly limit also consumes zero same-lane retries, excludes the exact lane for the current session, and follows the configured sibling then fallback route"
        ));
        assert!(manager.contains(
            "Do not classify generic text, generic 5xx, authentication failures, user cancellation, or ordinary transient errors as quota"
        ));
        assert!(
            !manager
                .contains("Quota, rate-limit, or capacity exhaustion gets zero same-lane retries")
        );
        assert!(
            !manager
                .contains("until reset evidence appears or the user explicitly requests a retry")
        );

        let policy_lower = OPENCODE_MANAGER_PHASE_POLICY.to_ascii_lowercase();
        assert!(!policy_lower.contains("opus"));
        assert!(!policy_lower.contains("sonnet"));

        let worker = &plan.files["glm"];
        assert!(worker.contains("model: \"opencode-go/glm-5.2\""));
        assert!(
            worker.contains("Roles: [\"simple\",\"mechanical\",\"boilerplate\",\"bulk-edit\"]")
        );
        assert!(worker.contains("Fallback lanes: [\"kimi\"]"));
        assert!(worker.contains("Return at most 3 summary lines"));
        assert!(!plan.files.contains_key("explore"));
        assert!(manager.contains("\"explore\": allow"));
        assert!(manager.contains("lane=\"explore\" host=\"explore\" roles=[\"discovery\"]"));
    }

    #[test]
    fn opencode_task_plan_reuses_builtins_and_rejects_model_override() {
        for builtin in ["explore", "general", "scout"] {
            let mut team = opencode_cloud_team();
            team.members.push(native_member(
                &format!("builtin-{builtin}"),
                builtin,
                None,
                &["discovery"],
                &[],
            ));
            let plan = build_opencode_task_agent_plan(&team).unwrap();
            assert!(!plan.files.contains_key(builtin), "{builtin}");
            assert!(plan.files[OPENCODE_MANAGER_AGENT].contains(&format!("\"{builtin}\": allow")));

            team.members.last_mut().unwrap().model = Some("ollama/custom:cloud".to_string());
            let error = build_opencode_task_agent_plan(&team).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("built-in OpenCode subagent {builtin}")),
                "{builtin}: {error}"
            );
        }

        for primary in ["build", "plan"] {
            let mut team = opencode_cloud_team();
            team.members.push(native_member(
                &format!("primary-{primary}"),
                primary,
                None,
                &["routine"],
                &[],
            ));
            let error = build_opencode_task_agent_plan(&team).unwrap_err();
            assert!(error.to_string().contains("primary-only"), "{error}");
        }
    }

    #[test]
    fn opencode_task_disabled_team_removes_only_managed_state() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        let team = opencode_cloud_team();
        install_opencode_task_agents_at(&agents, &config, &team, true).unwrap();
        std::fs::write(agents.join("user-agent.md"), "user owned\n").unwrap();
        let spoof = format!(
            "---\ndescription: spoof\nmode: subagent\n---\n{OPENCODE_AGENT_BEGIN}\nspoof\n{OPENCODE_AGENT_END}\n"
        );
        std::fs::write(agents.join("spoof.md"), &spoof).unwrap();

        let disabled = rtrt_core::TeamConfig {
            enabled: false,
            ..team
        };
        install_opencode_task_agents_at(&agents, &config, &disabled, true).unwrap();

        assert!(!agents.join(format!("{OPENCODE_MANAGER_AGENT}.md")).exists());
        assert_eq!(
            std::fs::read_to_string(agents.join("spoof.md")).unwrap(),
            spoof
        );
        assert_eq!(
            std::fs::read_to_string(agents.join("user-agent.md")).unwrap(),
            "user owned\n"
        );
        let restored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(restored.get("provider").is_none());
        assert!(restored.get("permission").is_none());
        assert!(restored.get("subagent_depth").is_none());
    }

    #[test]
    fn opencode_task_plan_accepts_hermetic_custom_team_config() {
        let config = rtrt_core::Config::from_toml_str(
            r#"
            [team]
            enabled = true
            manager_provider = "ollama"
            manager_model = "custom-manager:cloud"
            leader_order = ["custom"]

            [[team.members]]
            name = "custom"
            target = "opencode"
            model = "ollama/custom-worker:cloud"
            mode = "cli"
            roles = ["routine", "custom-role"]
            host_agent = "custom-worker"
            fallback = []
            "#,
        )
        .unwrap();

        let plan = build_opencode_task_agent_plan(&config.team).unwrap();

        assert_eq!(
            opencode_agent_model(&plan.files[OPENCODE_MANAGER_AGENT]),
            Some("ollama/custom-manager:cloud")
        );
        assert_eq!(
            opencode_agent_model(&plan.files["custom-worker"]),
            Some("ollama/custom-worker:cloud")
        );
        assert!(plan.files["custom-worker"].contains("custom-role"));
        assert_eq!(
            plan.ollama_models,
            BTreeSet::from([
                "custom-manager:cloud".to_string(),
                "custom-worker:cloud".to_string(),
            ])
        );
    }

    #[test]
    fn opencode_task_plan_renders_custom_cli_lane_as_safe_direct_claude_argv() {
        let config = rtrt_core::Config::from_toml_str(
            r#"
            [team]
            enabled = true
            manager_provider = "openai"
            manager_model = "gpt-5.6-sol"
            leader_order = ["native"]

            [team.tiers]
            hard = ["native"]
            review = ["custom-cli"]

            [[team.members]]
            name = "native"
            target = "opencode"
            model = "openai/gpt-5.6-sol"
            mode = "cli"
            roles = ["hard-implementation"]
            host_agent = "native-worker"
            fallback = ["custom-cli"]

            [[team.members]]
            name = "custom-cli"
            target = "claude"
            model = "sonnet"
            mode = "cli"
            roles = ["review", "custom"]
            delegation = "cli"

            [team.members.flags]
            output-format = "json"
            permission-mode = "plan"
            permission-prompt-tool = "mcp__rtrt__permission_prompt"
            "#,
        )
        .unwrap();

        let plan = build_opencode_task_agent_plan(&config.team).unwrap();
        let manager = &plan.files[OPENCODE_MANAGER_AGENT];
        let argv = "claude -p --model sonnet --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt '<PROMPT>'";

        assert!(manager.contains(&format!("argv_template={}", yaml_string(argv))));
        assert!(manager.contains(&format!(
            "fallback_routes=[{}]",
            yaml_string(&format!("custom-cli => {argv}"))
        )));
        assert!(manager.contains(&format!("- \"review\": [{}]", yaml_string(argv))));
        assert!(manager.contains(&format!(
            "{}: allow",
            yaml_string(&argv.replace("'<PROMPT>'", "'*'"))
        )));
        assert!(manager.contains("may not add `--allowed-tools`"));
        assert!(!plan.files.contains_key("custom-cli"));
    }

    #[test]
    fn opencode_cli_permission_prompt_is_exactly_once_and_rejects_unsafe_flags() {
        let mut team = opencode_cloud_team();
        let shell = team
            .members
            .iter_mut()
            .find(|member| member.delegation == rtrt_core::Delegation::Shell)
            .unwrap();
        shell.flags.insert(
            CLAUDE_PERMISSION_PROMPT_FLAG.to_string(),
            CLAUDE_PERMISSION_PROMPT_TOOL.to_string(),
        );
        let plan = build_opencode_task_agent_plan(&team).unwrap();
        let manager = &plan.files[OPENCODE_MANAGER_AGENT];
        let canonical =
            format!("--{CLAUDE_PERMISSION_PROMPT_FLAG} {CLAUDE_PERMISSION_PROMPT_TOOL}");
        let shell = team
            .members
            .iter()
            .find(|member| member.delegation == rtrt_core::Delegation::Shell)
            .unwrap();
        let argv = claude_shell_argv_template(shell);
        assert_eq!(argv.matches(&canonical).count(), 1, "{argv}");
        assert!(argv.ends_with("mcp__rtrt__permission_prompt '<PROMPT>'"));
        assert!(manager.contains("may not omit or replace `--permission-prompt-tool"));
        assert!(manager.contains("may not add `--allowed-tools`"));

        for (key, value) in [
            ("ALLOWED-TOOLS", "Read"),
            ("AllowedTools", "Read"),
            ("DANGEROUSLY-SKIP-PERMISSIONS", ""),
            ("Permission-Mode", "BYPASSPERMISSIONS"),
            ("Permission-Prompt-Tool", "foreign_tool"),
        ] {
            let mut rejected = opencode_cloud_team();
            rejected
                .members
                .iter_mut()
                .find(|member| member.delegation == rtrt_core::Delegation::Shell)
                .unwrap()
                .flags
                .insert(key.to_string(), value.to_string());
            assert!(build_opencode_task_agent_plan(&rejected).is_err(), "{key}");
        }
    }

    #[test]
    fn opencode_task_agent_modes_enforce_manager_worker_boundaries() {
        let team = rtrt_core::TeamConfig::preset(rtrt_core::RosterPreset::OpencodeLead);
        let plan = build_opencode_task_agent_plan(&team).unwrap();
        let manager = &plan.files[OPENCODE_MANAGER_AGENT];
        assert!(manager.contains("mode: primary"));
        assert!(!manager.contains("permission:\n  \"*\": deny"));
        assert!(manager.contains(
            "\"claude -p --model opus --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt '*'\": allow"
        ));
        assert!(manager.contains(
            "\"claude -p --model sonnet --output-format json --permission-mode acceptEdits --permission-prompt-tool mcp__rtrt__permission_prompt '*'\": allow"
        ));
        assert!(!manager.contains("claude -p --model opus --output-format json --permission-mode plan --permission-prompt-tool mcp__rtrt__permission_prompt 'unmatched'"));
        assert!(manager.contains("  bash:\n    \"*\": allow"));
        assert!(!manager.contains("cargo publish"));
        assert!(!manager.contains("\"rm *\""));
        assert!(!manager.contains("\"*>*\""));
        for forbidden in [
            "rtrt_agent_call: allow",
            "rtrt_agent_route: allow",
            "rtrt_team_dispatch: allow",
            "opencode *\": allow",
            "rtrt *\": allow",
            "bash: allow",
        ] {
            assert!(!manager.contains(forbidden), "{forbidden}");
        }
        assert!(!manager.contains("permission:\n  \"*\": deny"));
        assert!(!manager.contains("Never launch nested OpenCode, invoke RTRT agent/team bridges, or run any non-Claude shell command"));
        assert!(manager.contains("Bash is enabled only when setup verified strict confinement"));
        assert!(manager.contains(
            "Never wrap or obfuscate commands to evade matching, launch nested OpenCode, or invoke RTRT agent/team bridges"
        ));
        assert!(manager.contains("Respect rejection and cancellation immediately"));
        assert!(manager.contains("OpenCode `once` is one request"));
        assert!(manager.contains(
            "RTRT Claude bridge `always` is parent-session/process scoped and clears on session deletion or OpenCode restart"
        ));
        assert!(
            manager
                .contains("RTRT does not persist raw approvals or project-saved permission rules")
        );
        assert!(!manager.contains("always` permission may persist"));
        assert!(manager.contains("never add shell operators"));
        assert!(manager.contains("  edit: allow"));
        assert!(manager.contains("  external_directory: ask"));
        assert!(manager.contains("  rtrt_agent_call: deny"));
        assert!(manager.contains("  rtrt_team_dispatch: deny"));
        assert!(manager.contains("  task:\n    \"*\": deny"));
        assert!(manager.contains("    \"explore\": allow"));

        for name in ["glm", "kimi-k3", "codex-sol-worker"] {
            let worker = &plan.files[name];
            assert!(worker.contains("mode: subagent"), "{name}");
            for allowed in [
                "  read: allow",
                "  glob: allow",
                "  grep: allow",
                "  list: allow",
            ] {
                assert!(worker.contains(allowed), "{name}: {allowed}");
            }
            assert!(worker.contains("  bash:\n    \"*\": allow"), "{name}");
            assert!(!worker.contains("cargo publish"), "{name}");
            assert!(!worker.contains("\"rm *\""), "{name}");
            assert!(worker.contains("  task: deny"), "{name}");
            assert!(worker.contains("  external_directory: ask"), "{name}");
            assert!(worker.contains("  rtrt_agent_call: deny"), "{name}");
            assert!(worker.contains("  rtrt_agent_route: deny"), "{name}");
            assert!(worker.contains("  rtrt_team_dispatch: deny"), "{name}");
            assert!(worker.contains("OpenCode `once` is one request"), "{name}");
            assert!(worker.contains(
                "RTRT Claude bridge `always` is parent-session/process scoped and clears on session deletion or OpenCode restart"
            ), "{name}");
            assert!(
                worker.contains(
                    "RTRT does not persist raw approvals or project-saved permission rules"
                ),
                "{name}"
            );
            assert!(!worker.contains("always` permission may persist"), "{name}");
        }
    }

    #[test]
    fn opencode_manager_omits_bash_permission_without_claude_cli_lanes() {
        let team = rtrt_core::TeamConfig {
            enabled: true,
            manager_provider: "openai".to_string(),
            manager_model: "manager".to_string(),
            leader_order: vec!["worker".to_string()],
            members: vec![native_member("worker", "worker", None, &["routine"], &[])],
            ..rtrt_core::TeamConfig::default()
        };
        let plan = build_opencode_task_agent_plan(&team).unwrap();
        let manager = &plan.files[OPENCODE_MANAGER_AGENT];

        assert!(manager.contains("  bash:\n    \"*\": allow"));
        assert!(!manager.contains("permission:\n  \"*\": deny"));
        assert!(manager.contains("  task:\n    \"*\": deny"));
        assert!(manager.contains("    \"worker\": allow"));
    }

    #[test]
    fn opencode_worker_permissions_follow_config_changes() {
        let configured_member = |edit: &str, bash: &str| {
            let raw = format!(
                r#"
                [team]
                enabled = true
                manager_provider = "openai"
                manager_model = "manager"
                leader_order = ["worker"]

                [[team.members]]
                name = "worker"
                target = "opencode"
                mode = "cli"
                roles = ["unchanged-role"]
                host_agent = "worker"

                [team.members.permissions]
                edit = "{edit}"

                [team.members.permissions.bash]
                "verify --quick" = "{bash}"
                "#
            );
            rtrt_core::Config::from_toml_str(&raw)
                .unwrap()
                .team
                .members
                .remove(0)
        };
        let denied =
            render_opencode_worker_agent(&configured_member("deny", "deny"), None, 3, true);
        let allowed =
            render_opencode_worker_agent(&configured_member("allow", "allow"), None, 3, true);

        assert!(denied.contains("  edit: deny"));
        assert!(denied.contains("  bash:\n    \"*\": allow"));
        assert!(!denied.contains("cargo publish"));
        assert!(denied.contains("\"verify --quick\": deny"));
        assert!(allowed.contains("  edit: allow"));
        assert!(allowed.contains("  bash:\n    \"*\": allow"));
        assert!(allowed.contains("\"verify --quick\": allow"));
    }

    #[test]
    fn opencode_worker_empty_permissions_omit_optional_entries_and_read_only_wins() {
        let mut worker = native_member("worker", "worker", None, &[], &[]);
        worker.permissions = Some(rtrt_core::config::NativePermissions::default());

        let empty = render_opencode_worker_agent(&worker, None, 3, true);
        assert!(empty.contains("  edit: allow"));
        assert!(empty.contains("  bash:\n    \"*\": allow"));

        worker.permissions.as_mut().unwrap().edit =
            Some(rtrt_core::config::PermissionAction::Allow);
        worker.allow_impl = false;
        let read_only = render_opencode_worker_agent(&worker, None, 3, true);
        assert!(read_only.contains("  edit: deny"));
        assert!(!read_only.contains("  edit: allow"));
        assert!(read_only.contains("  bash:\n    \"*\": allow"));
    }

    #[test]
    fn opencode_worker_roles_do_not_infer_permissions() {
        let discovery = native_member("worker", "worker", None, &["discovery", "read-only"], &[]);
        let implementation = native_member(
            "worker",
            "worker",
            None,
            &["hard-implementation", "tests", "systems"],
            &[],
        );

        for member in [&discovery, &implementation] {
            let rendered = render_opencode_worker_agent(member, None, 3, true);
            assert!(rendered.contains("  edit: allow"));
            assert!(rendered.contains("  bash:\n    \"*\": allow"));
        }
    }

    #[test]
    fn opencode_worker_omitted_permissions_inherit_host_defaults_except_read_only() {
        let writable = native_member("write", "write", None, &[], &[]);
        let mut read_only = native_member("read", "read", None, &[], &[]);
        read_only.allow_impl = false;

        let writable = render_opencode_worker_agent(&writable, None, 3, true);
        let read_only = render_opencode_worker_agent(&read_only, None, 3, true);
        for allowed in [
            "  read: allow",
            "  glob: allow",
            "  grep: allow",
            "  list: allow",
        ] {
            assert!(writable.contains(allowed), "{allowed}");
        }
        assert!(writable.contains("  edit: allow"));
        assert!(writable.contains("  bash:\n    \"*\": allow"));
        assert!(read_only.contains("  edit: deny"));
        assert!(read_only.contains("  bash:\n    \"*\": allow"));
    }

    #[test]
    fn confined_workers_need_no_hardcoded_risky_command_rules() {
        let team = rtrt_core::TeamConfig::preset(rtrt_core::RosterPreset::OpencodeLead);

        for member in team.members.iter().filter(|member| {
            member.delegation == rtrt_core::Delegation::Native
                && member
                    .host_agent
                    .as_deref()
                    .is_some_and(|host| !is_builtin_opencode_subagent(host))
        }) {
            let rendered = render_opencode_worker_agent(member, None, 3, true);
            assert!(
                rendered.contains("  bash:\n    \"*\": allow"),
                "{}",
                member.name
            );
            assert!(!rendered.contains("cargo publish"), "{}", member.name);
            assert!(!rendered.contains("git push"), "{}", member.name);
        }
    }

    #[test]
    fn unconfined_agents_deny_all_bash_including_configured_allows() {
        let team = opencode_cloud_team();
        let plan = build_opencode_task_agent_plan_with_confinement(&team, false).unwrap();
        for body in plan.files.values() {
            assert!(body.contains("  bash:\n    \"*\": deny"));
            assert!(!body.contains("\"*\": allow"));
        }
    }

    #[test]
    fn opencode_task_dry_run_maps_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.jsonc");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = "{\n  // foreign\n  \"mcp\": {\"foreign\": {\"enabled\": false}},\n}\n";
        std::fs::write(&config, original).unwrap();

        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), false).unwrap();

        assert!(!agents.exists());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        assert!(!backup_path(&config).exists());
    }

    #[test]
    fn opencode_task_sync_merges_jsonc_and_is_idempotent_with_private_modes() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.jsonc");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = r#"{
  // preserve foreign values, comments, formatting, and trailing commas
  "mcp": {"foreign": {"type": "local", "command": ["foreign"],},},
  "provider": {
    "ollama": {
      "name": "Foreign Ollama",
      "npm": "@foreign/provider",
      "options": {"baseURL": "http://foreign.invalid/v1", "token": "keep"},
      "models": {
        "glm-5.2:cloud": {
          "name": "Existing GLM",
          "tool_call": false,
          "options": {"foreign": true,},
        },
      },
    },
    "foreign-provider": {"options": {"keep": true}}
  },
  "permission": {"bash": "ask", "task": {"foreign-agent": "ask",},},
  "subagent_depth": 0,
  "foreign": {"nested": [1, 2, 3],},
}
"#;
        std::fs::write(&config, original).unwrap();

        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();
        let first_config = std::fs::read_to_string(&config).unwrap();
        let first_state = std::fs::read_to_string(agents.join(OPENCODE_AGENT_STATE_FILE)).unwrap();
        let first_manager =
            std::fs::read_to_string(agents.join(format!("{OPENCODE_MANAGER_AGENT}.md"))).unwrap();
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();

        assert_eq!(std::fs::read_to_string(&config).unwrap(), first_config);
        assert_eq!(
            std::fs::read_to_string(agents.join(OPENCODE_AGENT_STATE_FILE)).unwrap(),
            first_state
        );
        let state: serde_json::Value = serde_json::from_str(&first_state).unwrap();
        assert_eq!(state["config_path"], config.to_string_lossy().as_ref());
        assert!(state.get("task_rules").is_none());
        assert!(
            state["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == OPENCODE_MANAGER_AGENT)
        );
        assert_eq!(
            std::fs::read_to_string(agents.join(format!("{OPENCODE_MANAGER_AGENT}.md"))).unwrap(),
            first_manager
        );
        assert!(first_config.contains("// preserve foreign values, comments"));
        assert!(first_config.contains("\"foreign-agent\": \"ask\","));
        let merged = parse_json_or_jsonc(&first_config, &config).unwrap();
        assert_eq!(merged["mcp"]["foreign"]["command"][0], "foreign");
        assert_eq!(merged["provider"]["ollama"]["name"], "Foreign Ollama");
        assert_eq!(merged["provider"]["ollama"]["options"]["token"], "keep");
        assert_eq!(
            merged["provider"]["ollama"]["models"]["glm-5.2:cloud"]["options"]["foreign"],
            true
        );
        for model in ["glm-5.2:cloud", "kimi-k2.7-code:cloud", "kimi-k3:cloud"] {
            assert_eq!(
                merged["provider"]["ollama"]["models"][model]["tool_call"], true,
                "{model}"
            );
        }
        assert_eq!(merged["permission"]["bash"], "ask");
        assert_eq!(merged["permission"]["task"]["foreign-agent"], "ask");
        assert_eq!(
            merged["permission"]["task"],
            serde_json::json!({"foreign-agent": "ask"})
        );
        assert_eq!(merged["subagent_depth"], 1);
        assert_eq!(merged["foreign"]["nested"], serde_json::json!([1, 2, 3]));
        assert!(merged.get("default_agent").is_none());
        assert_eq!(
            std::fs::read_to_string(backup_path(&config)).unwrap(),
            original
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&agents).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for name in opencode_state_agent_names(&state).unwrap() {
                let path = agents.join(format!("{name}.md"));
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            assert_eq!(
                std::fs::metadata(agents.join(OPENCODE_AGENT_STATE_FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            std::fs::read_dir(config.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".rtrt-"))
        );
    }

    #[test]
    fn opencode_task_rerun_reflects_team_changes_and_prunes_owned_agents() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        let mut team = opencode_cloud_team();
        install_opencode_task_agents_at(&agents, &config, &team, true).unwrap();
        std::fs::write(agents.join("user-agent.md"), "user owned\n").unwrap();

        let glm = team.member("glm").unwrap().clone();
        let glm_index = team
            .members
            .iter()
            .position(|member| member.name == "glm")
            .unwrap();
        team.members[glm_index] = rtrt_core::TeamMember {
            host_agent: Some("glm-v2".to_string()),
            model: Some("ollama/glm-5.2:new-cloud".to_string()),
            roles: vec!["simple".to_string(), "renamed-role".to_string()],
            fallback: vec!["kimi-cloud".to_string()],
            ..glm
        };
        let kimi_cloud_index = team
            .members
            .iter()
            .position(|member| member.name == "kimi-cloud")
            .unwrap();
        team.members[kimi_cloud_index]
            .roles
            .push("dashboard-change".to_string());

        install_opencode_task_agents_at(&agents, &config, &team, true).unwrap();

        assert!(!agents.join("glm.md").exists());
        let changed = std::fs::read_to_string(agents.join("glm-v2.md")).unwrap();
        assert!(changed.contains("model: \"opencode-go/glm-5.2:new-cloud\""));
        assert!(changed.contains("renamed-role"));
        assert!(changed.contains("Fallback lanes: [\"kimi-cloud\"]"));
        assert_eq!(
            std::fs::read_to_string(agents.join("user-agent.md")).unwrap(),
            "user owned\n"
        );
        assert!(backup_path(&agents.join("kimi-cloud.md")).exists());
        let manager =
            std::fs::read_to_string(agents.join(format!("{OPENCODE_MANAGER_AGENT}.md"))).unwrap();
        assert!(manager.contains("host=\"glm-v2\""));
        assert!(!manager.contains("lane=\"glm\" host=\"glm\""));
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(merged.get("provider").is_none());
        assert!(merged.get("permission").is_none());
    }

    #[test]
    fn opencode_task_ownership_requires_private_state_even_for_marked_files() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        std::fs::create_dir_all(&agents).unwrap();
        let manager_path = agents.join(format!("{OPENCODE_MANAGER_AGENT}.md"));
        std::fs::write(&manager_path, "user manager\n").unwrap();

        let error = install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true)
            .unwrap_err();
        assert!(error.to_string().contains("unknown same-name"));
        assert_eq!(
            std::fs::read_to_string(&manager_path).unwrap(),
            "user manager\n"
        );
        assert!(!config.exists());

        let old = format!(
            "---\ndescription: old\nmode: primary\n---\n{OPENCODE_AGENT_BEGIN}\nold version\n{OPENCODE_AGENT_END}\n"
        );
        std::fs::write(&manager_path, &old).unwrap();
        let error = install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without private ownership state")
        );
        assert_eq!(std::fs::read_to_string(&manager_path).unwrap(), old);

        std::fs::remove_file(&manager_path).unwrap();
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();
        std::fs::write(&manager_path, &old).unwrap();
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();
        assert_ne!(std::fs::read_to_string(&manager_path).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(backup_path(&manager_path)).unwrap(),
            old
        );
    }

    #[test]
    fn opencode_task_uninstall_restores_owned_fields_and_preserves_foreign_data() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        let plan = build_opencode_task_agent_plan(&opencode_cloud_team()).unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            serde_json::to_string_pretty(&serde_json::json!({
                "default_agent": "foreign-primary",
                "mcp": {"foreign": {"enabled": false}},
                "provider": {
                    "ollama": {
                        "options": {"baseURL": "http://keep.invalid/v1"},
                        "models": {
                            "glm-5.2:cloud": {
                                "name": "keep",
                                "tool_call": false
                            }
                        }
                    }
                },
                "permission": {
                    "bash": "ask",
                    "task": {"foreign": "ask", "glm": "ask"}
                },
                "subagent_depth": 0,
                "foreign": true
            }))
            .unwrap(),
        )
        .unwrap();
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();
        std::fs::write(agents.join("user-agent.md"), "preserve me\n").unwrap();

        remove_opencode_task_agents_at(&agents, &config, true).unwrap();

        for name in plan.files.keys() {
            assert!(!agents.join(format!("{name}.md")).exists(), "{name}");
        }
        assert_eq!(
            std::fs::read_to_string(agents.join("user-agent.md")).unwrap(),
            "preserve me\n"
        );
        assert!(!agents.join(OPENCODE_AGENT_STATE_FILE).exists());
        let restored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(restored["default_agent"], "foreign-primary");
        assert_eq!(restored["mcp"]["foreign"]["enabled"], false);
        assert_eq!(
            restored["provider"]["ollama"]["options"]["baseURL"],
            "http://keep.invalid/v1"
        );
        assert_eq!(
            restored["provider"]["ollama"]["models"]["glm-5.2:cloud"]["tool_call"],
            false
        );
        assert!(
            restored["provider"]["ollama"]["models"]
                .get("kimi-k2.7-code:cloud")
                .is_none()
        );
        assert!(
            restored["provider"]["ollama"]["models"]
                .get("kimi-k3:cloud")
                .is_none()
        );
        assert_eq!(restored["permission"]["bash"], "ask");
        assert_eq!(restored["permission"]["task"]["foreign"], "ask");
        assert_eq!(restored["permission"]["task"]["glm"], "ask");
        assert!(restored["permission"]["task"].get("explore").is_none());
        assert_eq!(restored["subagent_depth"], 0);
        assert_eq!(restored["foreign"], true);
    }

    #[test]
    fn opencode_task_uninstall_fails_when_modified_execution_agent_remains() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();
        let manager = agents.join(format!("{OPENCODE_MANAGER_AGENT}.md"));
        std::fs::write(&manager, "user-modified execution agent\n").unwrap();

        let error = remove_opencode_task_agents_at(&agents, &config, true).unwrap_err();
        assert!(error.to_string().contains("execution agent remains"));
        assert!(manager.exists());
        assert!(agents.join(OPENCODE_AGENT_STATE_FILE).exists());
    }

    #[test]
    fn opencode_task_rerun_restores_legacy_task_rules_without_touching_global_policy() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            serde_json::to_string_pretty(&serde_json::json!({
                "permission": {
                    "bash": "ask",
                    "task": {"foreign": "deny", "glm": "ask"}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();

        let mut installed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        installed["permission"]["task"]["glm"] = serde_json::json!("allow");
        std::fs::write(&config, serde_json::to_string_pretty(&installed).unwrap()).unwrap();
        let state_path = agents.join(OPENCODE_AGENT_STATE_FILE);
        let mut state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        state["task_rules"] = serde_json::json!({
            "glm": {"present": true, "value": "ask"}
        });
        std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();

        let restored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            restored["permission"],
            serde_json::json!({
                "bash": "ask",
                "task": {"foreign": "deny", "glm": "ask"}
            })
        );
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert!(state.get("task_rules").is_none());
        assert!(state.get("task_baseline").is_none());
        assert!(state.get("permission_baseline").is_none());
    }

    #[test]
    fn opencode_task_requires_and_preserves_operator_ollama_provider() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        let operator_provider = serde_json::json!({
            "name": "Operator Ollama",
            "npm": "operator/ollama-adapter",
            "options": {"baseURL": "https://operator.invalid/v1", "token": "keep"},
            "foreign": {"keep": true}
        });
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            serde_json::to_string_pretty(&serde_json::json!({
                "provider": {"ollama": operator_provider}, "foreign": true
            }))
            .unwrap(),
        )
        .unwrap();
        install_opencode_task_agents_at_with_confinement(
            &agents,
            &config,
            &opencode_cloud_team(),
            true,
            true,
        )
        .unwrap();
        remove_opencode_task_agents_at(&agents, &config, true).unwrap();
        let preserved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(preserved["provider"]["ollama"]["name"], "Operator Ollama");
        assert_eq!(
            preserved["provider"]["ollama"]["npm"],
            "operator/ollama-adapter"
        );
        assert_eq!(preserved["provider"]["ollama"]["foreign"]["keep"], true);
    }

    #[test]
    fn opencode_task_missing_ollama_provider_fails_before_writes_in_dry_run_and_apply() {
        for apply in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let agents = dir.path().join("opencode/agents");
            let config = dir.path().join("opencode/opencode.json");
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::write(&config, "{}\n").unwrap();
            let before = std::fs::read_to_string(&config).unwrap();
            let error = install_opencode_task_agents_at_with_confinement(
                &agents,
                &config,
                &opencode_cloud_team(),
                apply,
                true,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("explicitly configure and approve provider.ollama")
            );
            assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
            assert!(!agents.exists());
        }
    }

    #[test]
    fn opencode_task_legacy_rtrt_provider_is_removed_then_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            serde_json::to_string(&serde_json::json!({
                "provider": {"ollama": legacy_rtrt_ollama_provider()}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join(OPENCODE_AGENT_STATE_FILE),
            serde_json::to_string(&serde_json::json!({
                "owner": OPENCODE_AGENT_STATE_OWNER,
                "version": 1,
                "models": {},
                "ollama_created": true,
                "models_created": true
            }))
            .unwrap(),
        )
        .unwrap();
        let error = install_opencode_task_agents_at_with_confinement(
            &agents,
            &config,
            &opencode_cloud_team(),
            true,
            true,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("legacy RTRT-owned provider.ollama"),
            "{error}"
        );
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(root.get("provider").is_none());
        assert!(!agents.join(format!("{OPENCODE_MANAGER_AGENT}.md")).exists());
    }

    #[test]
    fn opencode_task_state_precedes_config_write_and_records_exact_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "{}\n").unwrap();
        std::fs::create_dir(backup_path(&config)).unwrap();

        let error = install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true)
            .unwrap_err();
        assert!(error.to_string().contains("backup destination"), "{error}");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "{}\n");
        let state_path = agents.join(OPENCODE_AGENT_STATE_FILE);
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(state["config_path"], config.to_string_lossy().as_ref());
        let names: BTreeSet<&str> = state["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(names.contains(OPENCODE_MANAGER_AGENT));
        assert!(names.contains("glm"));
        assert!(!agents.join(format!("{OPENCODE_MANAGER_AGENT}.md")).exists());
    }

    #[test]
    fn opencode_task_uninstall_prefers_recorded_json_across_jsonc_switch() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let json = dir.path().join("opencode/opencode.json");
        let jsonc = dir.path().join("opencode/opencode.jsonc");
        install_opencode_task_agents_at(&agents, &json, &opencode_cloud_team(), true).unwrap();
        let jsonc_original = "{\n  // current resolver target\n  \"foreign\": true,\n}\n";
        std::fs::write(&jsonc, jsonc_original).unwrap();

        remove_opencode_task_agents_at(&agents, &jsonc, true).unwrap();

        assert_eq!(std::fs::read_to_string(&jsonc).unwrap(), jsonc_original);
        let restored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert!(restored.get("provider").is_none());
        assert!(restored.get("subagent_depth").is_none());
    }

    #[test]
    fn opencode_task_uninstall_rejects_state_config_outside_exact_json_paths() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("opencode/agents");
        let config = dir.path().join("opencode/opencode.json");
        install_opencode_task_agents_at(&agents, &config, &opencode_cloud_team(), true).unwrap();
        let state_path = agents.join(OPENCODE_AGENT_STATE_FILE);
        let mut state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        state["config_path"] = serde_json::json!(dir.path().join("foreign.json").to_string_lossy());
        std::fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        let error = remove_opencode_task_agents_at(&agents, &config, true).unwrap_err();
        assert!(error.to_string().contains("outside the resolved home"));
        assert!(agents.join(format!("{OPENCODE_MANAGER_AGENT}.md")).exists());
        assert!(state_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn opencode_task_rejects_symlink_config_backup_state_and_agent_destinations() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let team = opencode_cloud_team();

        let config_root = dir.path().join("config-link");
        std::fs::create_dir_all(&config_root).unwrap();
        let config_target = config_root.join("target.json");
        std::fs::write(&config_target, "{}\n").unwrap();
        let config_link = config_root.join("opencode.json");
        symlink(&config_target, &config_link).unwrap();
        let error =
            install_opencode_task_agents_at(&config_root.join("agents"), &config_link, &team, true)
                .unwrap_err();
        assert!(error.to_string().contains("not a real file"), "{error}");
        assert_eq!(std::fs::read_to_string(&config_target).unwrap(), "{}\n");

        let backup_root = dir.path().join("backup-link");
        std::fs::create_dir_all(&backup_root).unwrap();
        let backup_config = backup_root.join("opencode.json");
        std::fs::write(&backup_config, "{}\n").unwrap();
        let backup_target = backup_root.join("backup-target");
        std::fs::write(&backup_target, "foreign backup\n").unwrap();
        symlink(&backup_target, backup_path(&backup_config)).unwrap();
        let error = install_opencode_task_agents_at(
            &backup_root.join("agents"),
            &backup_config,
            &team,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("backup destination"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&backup_target).unwrap(),
            "foreign backup\n"
        );

        let state_root = dir.path().join("state-link/agents");
        std::fs::create_dir_all(&state_root).unwrap();
        let state_target = dir.path().join("state-target");
        std::fs::write(&state_target, "foreign state\n").unwrap();
        symlink(&state_target, state_root.join(OPENCODE_AGENT_STATE_FILE)).unwrap();
        let error = install_opencode_task_agents_at(
            &state_root,
            &dir.path().join("state-link/opencode.json"),
            &team,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("state file"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&state_target).unwrap(),
            "foreign state\n"
        );

        let agent_root = dir.path().join("agent-link/agents");
        std::fs::create_dir_all(&agent_root).unwrap();
        let agent_target = dir.path().join("agent-target");
        std::fs::write(&agent_target, "foreign agent\n").unwrap();
        symlink(
            &agent_target,
            agent_root.join(format!("{OPENCODE_MANAGER_AGENT}.md")),
        )
        .unwrap();
        let error = install_opencode_task_agents_at(
            &agent_root,
            &dir.path().join("agent-link/opencode.json"),
            &team,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink at managed agent name"));
        assert_eq!(
            std::fs::read_to_string(&agent_target).unwrap(),
            "foreign agent\n"
        );
    }

    #[test]
    fn opencode_uninstall_runs_all_surfaces_and_aggregates_errors() {
        let mut visited = Vec::new();
        let error = run_opencode_uninstall_steps(|surface| {
            visited.push(surface);
            if surface == OpenCodeUninstallSurface::TaskAgents {
                bail!("task failure");
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(visited.len(), 5);
        assert_eq!(visited[0], OpenCodeUninstallSurface::TaskAgents);
        assert_eq!(visited[2], OpenCodeUninstallSurface::McpConfig);
        assert_eq!(visited[3], OpenCodeUninstallSurface::ProvenancePlugin);
        assert!(!visited.contains(&OpenCodeUninstallSurface::Sandbox));
        assert!(error.to_string().contains("2 error(s)"));
        assert!(error.to_string().contains("Task agents: task failure"));
        assert!(error.to_string().contains("sandbox: retained"));
    }

    #[test]
    fn opencode_uninstall_restores_shell_only_after_execution_agents_are_removed() {
        let mut visited = Vec::new();
        run_opencode_uninstall_steps(|surface| {
            visited.push(surface);
            Ok(())
        })
        .unwrap();
        assert_eq!(visited.first(), Some(&OpenCodeUninstallSurface::TaskAgents));
        assert_eq!(visited.last(), Some(&OpenCodeUninstallSurface::Sandbox));
    }

    #[test]
    fn opencode_uninstall_keeps_sandbox_after_any_prior_surface_failure() {
        let mut visited = Vec::new();
        let error = run_opencode_uninstall_steps(|surface| {
            visited.push(surface);
            if surface == OpenCodeUninstallSurface::ProvenancePlugin {
                bail!("plugin removal failed");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(!visited.contains(&OpenCodeUninstallSurface::Sandbox));
        assert!(error.to_string().contains("sandbox: retained"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_sandbox_registry_supports_two_projects_and_safe_last_restore() {
        use std::os::unix::fs::PermissionsExt;

        let temp = crate::sandbox::private_scratch();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = temp.path().join("opencode.jsonc");
        let registry = temp.path().join("registry.json");
        std::fs::write(
            &config,
            "{\n // keep\n \"shell\": \"/bin/zsh\", \"foreign\": true,\n}\n",
        )
        .unwrap();
        let executable = temp.path().join("rtrt");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = std::fs::canonicalize(executable).unwrap();
        let backend = PathBuf::from("/fixed/bwrap");
        let make_boundary = |name: &str| {
            let root = temp.path().join(name);
            std::fs::create_dir(&root).unwrap();
            crate::sandbox::ProjectBoundary {
                root: std::fs::canonicalize(&root).unwrap(),
                cwd: std::fs::canonicalize(&root).unwrap(),
                git_writable: Vec::new(),
            }
        };
        let first = make_boundary("first");
        let second = make_boundary("second");
        let unknown = make_boundary("unknown");

        enable_opencode_sandbox_at_registry(&first, &config, &executable, &backend, &registry)
            .unwrap();
        enable_opencode_sandbox_at_registry(&second, &config, &executable, &backend, &registry)
            .unwrap();
        let configured = std::fs::read_to_string(&config).unwrap();
        assert!(configured.contains("// keep"));
        assert!(configured.contains(executable.to_str().unwrap()));

        disable_opencode_sandbox_at_registry(&first, true, &registry, &executable).unwrap();
        assert!(registry.exists());
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains(executable.to_str().unwrap())
        );
        let registry_before = std::fs::read_to_string(&registry).unwrap();
        assert!(
            disable_opencode_sandbox_at_registry(&unknown, true, &registry, &executable).is_err()
        );
        assert_eq!(std::fs::read_to_string(&registry).unwrap(), registry_before);

        disable_opencode_sandbox_at_registry(&second, true, &registry, &executable).unwrap();
        assert!(!registry.exists());
        let restored = std::fs::read_to_string(&config).unwrap();
        assert!(restored.contains("// keep"));
        assert!(
            restored.contains("\"shell\":\"/bin/zsh\"")
                || restored.contains("\"shell\": \"/bin/zsh\"")
        );

        enable_opencode_sandbox_at_registry(&first, &config, &executable, &backend, &registry)
            .unwrap();
        let registry_before = std::fs::read_to_string(&registry).unwrap();
        std::fs::write(&config, r#"{"shell":"/foreign","keep":true}"#).unwrap();
        assert!(
            disable_opencode_sandbox_at_registry(&first, true, &registry, &executable).is_err()
        );
        assert_eq!(std::fs::read_to_string(&registry).unwrap(), registry_before);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            r#"{"shell":"/foreign","keep":true}"#
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opencode_machine_bootstrap_is_empty_repeatable_locked_and_globally_uninstallable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = crate::sandbox::private_scratch();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = temp.path().join("opencode.json");
        let registry = temp.path().join("registry.json");
        std::fs::write(&config, r#"{"shell":"/bin/zsh","foreign":true}"#).unwrap();
        let executable = temp.path().join("rtrt");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let executable = std::fs::canonicalize(executable).unwrap();
        let backend = PathBuf::from("/fixed/bwrap");

        enable_opencode_machine_sandbox_at(&config, &executable, &backend, &registry).unwrap();
        let first = crate::sandbox::load_registry_from(&registry, &executable).unwrap();
        assert_eq!(first["projects"], serde_json::json!({}));
        assert_eq!(first["prior_shell"], "/bin/zsh");

        let config_a = config.clone();
        let executable_a = executable.clone();
        let backend_a = backend.clone();
        let registry_a = registry.clone();
        let worker = std::thread::spawn(move || {
            enable_opencode_machine_sandbox_at(&config_a, &executable_a, &backend_a, &registry_a)
        });
        enable_opencode_machine_sandbox_at(&config, &executable, &backend, &registry).unwrap();
        worker.join().unwrap().unwrap();
        let repeated = crate::sandbox::load_registry_from(&registry, &executable).unwrap();
        assert_eq!(repeated["projects"], first["projects"]);
        assert_eq!(repeated["prior_shell"], first["prior_shell"]);

        let registry_before = std::fs::read(&registry).unwrap();
        std::fs::write(&config, r#"{"shell":"/tampered","foreign":true}"#).unwrap();
        assert!(
            enable_opencode_machine_sandbox_at(&config, &executable, &backend, &registry).is_err()
        );
        assert_eq!(std::fs::read(&registry).unwrap(), registry_before);

        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "shell": executable.to_string_lossy(),
                "foreign": true
            }))
            .unwrap(),
        )
        .unwrap();
        disable_opencode_sandbox_global_at(true, &registry, &executable).unwrap();
        assert!(!registry.exists());
        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(restored["shell"], "/bin/zsh");
        assert_eq!(restored["foreign"], true);
    }

    #[test]
    fn opencode_setup_runs_task_agents_last_and_stops_before_privilege_on_failure() {
        let mut visited = Vec::new();
        run_opencode_setup_steps(|surface| {
            visited.push(surface);
            Ok(())
        })
        .unwrap();
        assert_eq!(visited[0], OpenCodeSetupSurface::ProvenancePlugin);
        assert!(
            visited
                .iter()
                .position(|surface| *surface == OpenCodeSetupSurface::ProvenancePlugin)
                < visited
                    .iter()
                    .position(|surface| *surface == OpenCodeSetupSurface::McpConfig)
        );
        assert_eq!(visited.last(), Some(&OpenCodeSetupSurface::TaskAgents));
        assert_eq!(
            visited.get(visited.len() - 2),
            Some(&OpenCodeSetupSurface::McpConfig)
        );

        visited.clear();
        let error = run_opencode_setup_steps(|surface| {
            visited.push(surface);
            if surface == OpenCodeSetupSurface::TuiStatusline {
                bail!("surface failure");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("surface failure"));
        assert_eq!(visited.last(), Some(&OpenCodeSetupSurface::TuiStatusline));
        assert!(!visited.contains(&OpenCodeSetupSurface::TaskAgents));
    }

    #[test]
    fn opencode_provenance_plugin_covers_mcp_and_direct_shell_calls() {
        assert!(is_whole_file_managed_provenance_plugin(
            OPENCODE_PROVENANCE_PLUGIN
        ));
        for expected in [
            "rtrt_agent_call",
            "rtrt_agent_route",
            "rtrt_team_dispatch",
            "tool.execute.before",
            "shell.env",
            "RTRT_INVOCATION_ID",
            "RTRT_PARENT_SESSION_ID",
            "TMPDIR",
            "TEMP",
            "TMP",
            ".rtrt",
            "tmp",
            "opencode",
        ] {
            assert!(
                OPENCODE_PROVENANCE_PLUGIN.contains(expected),
                "plugin should contain {expected}"
            );
        }
        assert!(
            !OPENCODE_PROVENANCE_PLUGIN.contains("??="),
            "plugin must overwrite model-supplied provenance"
        );
    }

    #[test]
    fn opencode_provenance_plugin_upgrades_exact_legacy_v1_and_backs_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        assert_eq!(OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1.len(), 2_139);
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1).unwrap();

        install_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1
        );
    }

    #[test]
    fn opencode_provenance_plugin_upgrades_exact_legacy_v2_and_backs_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        assert_eq!(OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2.len(), 6_780);
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2).unwrap();

        install_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN
        );
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2
        );
        let state_path = opencode_provenance_state_path(&path).unwrap();
        assert_eq!(
            load_opencode_provenance_state(&state_path, &path).unwrap(),
            Some(OPENCODE_PROVENANCE_PLUGIN.to_string())
        );
    }

    #[test]
    fn opencode_provenance_plugin_auto_upgrades_unlisted_whole_file_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let prior = OPENCODE_PROVENANCE_PLUGIN.replace(
            "const RTRT_AGENT_TOOLS = new Set([",
            "// prior managed release\nconst RTRT_AGENT_TOOLS = new Set([",
        );
        assert!(is_whole_file_managed_provenance_plugin(&prior));
        std::fs::write(&path, &prior).unwrap();

        install_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN
        );
        assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), prior);
    }

    #[test]
    fn opencode_provenance_plugin_refuses_one_byte_legacy_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let mut edited = OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1.as_bytes().to_vec();
        edited[0] = b'I';
        std::fs::write(&path, &edited).unwrap();

        let error = install_opencode_provenance_plugin_at(&path, true).unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(std::fs::read(&path).unwrap(), edited);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_provenance_plugin_refuses_one_byte_legacy_v2_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let mut edited = OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2.as_bytes().to_vec();
        let last = edited.len() - 1;
        edited[last] = b'!';
        std::fs::write(&path, &edited).unwrap();

        let error = install_opencode_provenance_plugin_at(&path, true).unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(std::fs::read(&path).unwrap(), edited);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_provenance_plugin_current_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN).unwrap();

        install_opencode_provenance_plugin_at(&path, true).unwrap();
        install_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN
        );
        assert!(!backup_path(&path).exists());
        let state_path = opencode_provenance_state_path(&path).unwrap();
        assert_eq!(
            load_opencode_provenance_state(&state_path, &path).unwrap(),
            Some(OPENCODE_PROVENANCE_PLUGIN.to_string())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(state_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn opencode_provenance_plugin_upgrades_content_authorized_by_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        let prior = OPENCODE_PROVENANCE_PLUGIN.replace(
            "const RTRT_AGENT_TOOLS = new Set([",
            "// future installed release\nconst RTRT_AGENT_TOOLS = new Set([",
        );
        std::fs::write(&path, &prior).unwrap();
        write_opencode_provenance_state(&state_path, &path, &prior).unwrap();

        install_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN
        );
        assert_eq!(
            load_opencode_provenance_state(&state_path, &path).unwrap(),
            Some(OPENCODE_PROVENANCE_PLUGIN.to_string())
        );
    }

    #[test]
    fn opencode_provenance_plugin_refuses_modified_content_when_state_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        install_opencode_provenance_plugin_at(&path, true).unwrap();
        let modified = OPENCODE_PROVENANCE_PLUGIN.replace("rtrt_agent_call", "user_agent_call");
        std::fs::write(&path, &modified).unwrap();

        let error = install_opencode_provenance_plugin_at(&path, true).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("differs from private ownership state")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), modified);
    }

    #[test]
    fn opencode_provenance_plugin_refuses_invalid_and_path_mismatched_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN).unwrap();
        std::fs::write(&state_path, "[]").unwrap();
        assert!(install_opencode_provenance_plugin_at(&path, true).is_err());

        let state = serde_json::json!({
            "owner": OPENCODE_PROVENANCE_STATE_OWNER,
            "version": OPENCODE_PROVENANCE_STATE_VERSION,
            "path": dir.path().join("other.js").to_str().unwrap(),
            "content": OPENCODE_PROVENANCE_PLUGIN,
        });
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(install_opencode_provenance_plugin_at(&path, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN
        );
    }

    #[cfg(unix)]
    #[test]
    fn opencode_provenance_plugin_refuses_symlink_plugin_and_state() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let target = dir.path().join("target.js");
        std::fs::write(&target, OPENCODE_PROVENANCE_PLUGIN).unwrap();
        symlink(&target, &path).unwrap();
        assert!(install_opencode_provenance_plugin_at(&path, true).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN).unwrap();

        let state_path = opencode_provenance_state_path(&path).unwrap();
        let state_target = dir.path().join("state-target.json");
        std::fs::write(&state_target, "{}").unwrap();
        symlink(&state_target, &state_path).unwrap();
        assert!(install_opencode_provenance_plugin_at(&path, true).is_err());
    }

    #[test]
    fn opencode_provenance_plugin_strict_marker_bootstrap_rejects_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let malformed = [
            OPENCODE_PROVENANCE_PLUGIN.replacen(OPENCODE_PROVENANCE_PLUGIN_BEGIN, "// missing", 1),
            OPENCODE_PROVENANCE_PLUGIN.replace(
                OPENCODE_PROVENANCE_PLUGIN_END,
                &format!("{OPENCODE_PROVENANCE_PLUGIN_END}\n{OPENCODE_PROVENANCE_PLUGIN_END}"),
            ),
            format!("foreign prefix\n{OPENCODE_PROVENANCE_PLUGIN}"),
            format!("{OPENCODE_PROVENANCE_PLUGIN}foreign suffix\n"),
        ];
        for content in malformed {
            std::fs::write(&path, &content).unwrap();
            assert!(install_opencode_provenance_plugin_at(&path, true).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn opencode_provenance_plugin_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");

        install_opencode_provenance_plugin_at(&path, false).unwrap();

        assert!(!path.exists());
        assert!(!opencode_provenance_state_path(&path).unwrap().exists());
    }

    #[test]
    fn opencode_provenance_plugin_refuses_unknown_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let user_plugin = "export const UserPlugin = async () => ({})\n";
        std::fs::write(&path, user_plugin).unwrap();

        let error = install_opencode_provenance_plugin_at(&path, true).unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user_plugin);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_restores_safe_foreign_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let prior = "export const PriorPlugin = async () => ({})\n";
        std::fs::write(backup_path(&path), prior).unwrap();
        std::fs::write(&path, managed_opencode_provenance_plugin()).unwrap();

        remove_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), prior);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_preserves_modified_managed_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let modified = format!(
            "{}export const UserAddition = true\n",
            managed_opencode_provenance_plugin()
        );
        std::fs::write(&path, &modified).unwrap();

        remove_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), modified);
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_removes_exact_legacy_v2() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2).unwrap();

        remove_opencode_provenance_plugin_at(&path, true).unwrap();

        assert!(!path.exists());
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_cleans_state_but_preserves_state_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        install_opencode_provenance_plugin_at(&path, true).unwrap();
        remove_opencode_provenance_plugin_at(&path, true).unwrap();
        assert!(!path.exists());
        assert!(!state_path.exists());

        install_opencode_provenance_plugin_at(&path, true).unwrap();
        let modified = OPENCODE_PROVENANCE_PLUGIN.replace("rtrt_agent_call", "user_agent_call");
        std::fs::write(&path, &modified).unwrap();
        remove_opencode_provenance_plugin_at(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), modified);
        assert!(state_path.exists());
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_never_restores_managed_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let old = OPENCODE_PROVENANCE_PLUGIN.replace(
            "const RTRT_AGENT_TOOLS = new Set([",
            "// old managed backup\nconst RTRT_AGENT_TOOLS = new Set([",
        );
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN).unwrap();
        std::fs::write(backup_path(&path), old).unwrap();

        remove_opencode_provenance_plugin_at(&path, true).unwrap();

        assert!(!path.exists());
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_agents_rules_are_idempotent_and_uninstall_surgically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("AGENTS.md");
        let user_rules = "user rule\ncustom rule\n";
        std::fs::write(&path, user_rules).unwrap();

        install_opencode_agents_rules_at(&path, true).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        install_opencode_agents_rules_at(&path, true).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(second.matches(OPENCODE_WORKSPACE_BLOCK_BEGIN).count(), 1);
        assert!(second.contains("project `.rtrt/tmp`"));
        assert!(second.contains("never `/tmp` unless no project exists"));
        assert!(second.contains("Independent writes may run in parallel"));
        assert!(second.contains("overlapping writes must serialize"));
        assert!(second.contains("Native Task internal worktree placement remains host-owned"));
        assert!(second.contains("does not claim enforcement"));

        remove_opencode_agents_rules_at(&path, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user_rules);
    }

    #[test]
    fn opencode_agents_rules_preserve_modified_managed_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "user rule\n").unwrap();
        install_opencode_agents_rules_at(&path, true).unwrap();
        let modified = std::fs::read_to_string(&path)
            .unwrap()
            .replace("OpenCode workspace rules:", "User-edited workspace rules:");
        std::fs::write(&path, &modified).unwrap();

        assert!(install_opencode_agents_rules_at(&path, true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), modified);
        remove_opencode_agents_rules_at(&path, true).unwrap();
        let uninstalled = std::fs::read_to_string(&path).unwrap();
        assert!(uninstalled.contains("user rule"));
        assert!(uninstalled.contains("User-edited workspace rules:"));
        assert!(!uninstalled.contains(TERSE_BLOCK_BEGIN));
    }

    #[test]
    fn opencode_provenance_bridge_exact_file_cleanup_preserves_foreign_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {
                    "SessionStart": [{
                        "matcher": "*",
                        "hooks": [
                            {"type": "command", "command": "user before"},
                            {"type": "command", "command": "/legacy path/rtrt hook provenance --owner opencode"},
                            {"type": "command", "command": "user after"},
                            {"type": "command", "command": "/usr/bin/not-rtrt hook provenance --owner opencode"}
                        ]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        remove_opencode_provenance_bridge_at(&settings, true).unwrap();
        let removed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let entries = removed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let hooks = entries[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3);
        assert_eq!(hooks[0]["command"], "user before");
        assert_eq!(hooks[1]["command"], "user after");
        assert_eq!(
            hooks[2]["command"],
            "/usr/bin/not-rtrt hook provenance --owner opencode"
        );
    }

    #[test]
    fn provenance_exec_form_recognizes_windows_executable_paths() {
        let command = r"C:\Program Files\RTRT & Tools\rtrt.exe";
        let entry = provenance_hook_entry(command, "opencode");
        assert!(provenance_hook_matches(&entry["hooks"][0], "opencode"));
        assert_eq!(
            sibling_rtrt_binary(Path::new("/Program Files/RTRT/rtrt-cli.exe")),
            Some(PathBuf::from("/Program Files/RTRT/rtrt.exe"))
        );
    }

    #[test]
    fn resolve_opencode_config_path_prefers_json_when_both_exist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opencode_dir = dir.path().join(".config/opencode");
        std::fs::create_dir_all(&opencode_dir).unwrap();
        std::fs::write(opencode_dir.join("opencode.json"), "{}").unwrap();
        std::fs::write(opencode_dir.join("opencode.jsonc"), "{}").unwrap();

        let resolved = resolve_opencode_config_path_in(dir.path());

        assert_eq!(resolved, opencode_dir.join("opencode.json"));
    }

    #[test]
    fn resolve_opencode_config_path_falls_back_to_jsonc_when_only_it_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opencode_dir = dir.path().join(".config/opencode");
        std::fs::create_dir_all(&opencode_dir).unwrap();
        std::fs::write(opencode_dir.join("opencode.jsonc"), "{}").unwrap();

        let resolved = resolve_opencode_config_path_in(dir.path());

        assert_eq!(resolved, opencode_dir.join("opencode.jsonc"));
    }

    #[test]
    fn resolve_opencode_config_path_defaults_to_json_when_neither_exists() {
        let dir = tempfile::tempdir().expect("tempdir");

        let resolved = resolve_opencode_config_path_in(dir.path());

        assert_eq!(resolved, dir.path().join(".config/opencode/opencode.json"));
    }

    #[cfg(unix)]
    #[test]
    fn opencode_plugin_file_url_is_absolute_normalized_and_encoded() {
        let config = Path::new("/home/test/Config space/#100%/../한글/opencode.json");
        assert_eq!(
            opencode_provenance_plugin_url(config).unwrap(),
            "file:///home/test/Config%20space/%ED%95%9C%EA%B8%80/plugins/rtrt-provenance.js"
        );
        assert_eq!(
            opencode_provenance_plugin_url(Path::new("/etc/opencode/opencode.json")).unwrap(),
            "file:///etc/opencode/plugins/rtrt-provenance.js"
        );
    }

    #[cfg(unix)]
    #[test]
    fn opencode_plugin_file_url_encodes_hash_percent_and_question_mark() {
        assert_eq!(
            opencode_provenance_plugin_url(Path::new("/opt/a #/%25?/opencode.json")).unwrap(),
            "file:///opt/a%20%23/%2525%3F/plugins/rtrt-provenance.js"
        );
    }

    #[cfg(windows)]
    #[test]
    fn opencode_plugin_file_url_supports_windows_drive_paths() {
        assert_eq!(
            opencode_provenance_plugin_url(Path::new(r"C:\Users\Test User\#rtrt\opencode.json"))
                .unwrap(),
            "file:///C:/Users/Test%20User/%23rtrt/plugins/rtrt-provenance.js"
        );
    }

    #[cfg(windows)]
    #[test]
    fn opencode_plugin_file_url_conservatively_rejects_unc_paths() {
        let error =
            opencode_provenance_plugin_url(Path::new(r"\\server\share\opencode\opencode.json"))
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("UNC/device plugin paths are not supported")
        );
    }

    #[cfg(unix)]
    #[test]
    fn opencode_registration_dry_run_status_names_exact_url() {
        let url =
            opencode_provenance_plugin_url(Path::new("/home/test/OpenCode Config/opencode.json"))
                .unwrap();
        assert_eq!(
            opencode_provenance_dry_run_status(&url),
            "[dry-run] provenance plugin auto-loads; would remove legacy registrations file:///home/test/OpenCode%20Config/plugins/rtrt-provenance.js and ./plugins/rtrt-provenance.js"
        );
    }

    /// Writes an opencode config with a pre-existing `mcp.other` server to a
    /// temp file (never touches the real `~/.config/opencode`), applies the
    /// rtrt entry, and checks the result is valid JSON with `mcp.other`
    /// intact alongside a well-formed `mcp.rtrt`.
    #[test]
    fn opencode_apply_merges_without_dropping_other_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode.jsonc");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "mcp": { "other": { "type": "local", "command": ["other-server"] } }
            }))
            .unwrap(),
        )
        .expect("write starting config");

        let memory_path = Some(PathBuf::from("/home/u/.rtrt/memory.sqlite"));
        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &memory_path)
            .expect("apply should succeed");

        let raw = std::fs::read_to_string(&path).expect("read merged config");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("must be valid JSON");
        assert!(
            parsed.get("mcp").and_then(|m| m.get("other")).is_some(),
            "pre-existing mcp.other must survive the merge"
        );
        let rtrt = parsed
            .get("mcp")
            .and_then(|m| m.get("rtrt"))
            .expect("mcp.rtrt must be present");
        assert_eq!(rtrt.get("type").and_then(|v| v.as_str()), Some("local"));
        assert_eq!(rtrt.get("enabled").and_then(|v| v.as_bool()), Some(true));
        let command: Vec<&str> = rtrt
            .get("command")
            .and_then(|v| v.as_array())
            .expect("command must be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            command,
            vec![
                "/usr/local/bin/rtrt-mcp",
                "--memory",
                "/home/u/.rtrt/memory.sqlite",
            ]
        );
    }

    #[test]
    fn opencode_apply_idempotent_on_second_apply() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode.jsonc");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "mcp": { "other": { "type": "local", "command": ["other-server"] } }
            }))
            .unwrap(),
        )
        .expect("write starting config");

        let memory_path = Some(PathBuf::from("/home/u/.rtrt/memory.sqlite"));
        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &memory_path)
            .expect("first apply should succeed");
        let first = std::fs::read_to_string(&path).expect("read after first apply");

        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &memory_path)
            .expect("second apply should succeed");
        let second = std::fs::read_to_string(&path).expect("read after second apply");

        assert_eq!(first, second, "re-running apply must not change the file");
        let parsed: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert!(parsed.get("mcp").and_then(|m| m.get("other")).is_some());
        assert!(parsed.get("mcp").and_then(|m| m.get("rtrt")).is_some());
        assert!(parsed.get("plugin").is_none());
    }

    #[test]
    fn opencode_apply_creates_config_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Nested, not-yet-created parent — apply must mkdir -p it.
        let path = dir.path().join("nested/opencode.jsonc");

        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &None)
            .expect("apply should create the file and its parent dir");

        let raw = std::fs::read_to_string(&path).expect("read created config");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("must be valid JSON");
        assert_eq!(
            parsed
                .get("mcp")
                .and_then(|m| m.get("rtrt"))
                .and_then(|r| r.get("command"))
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str()),
            Some("/usr/local/bin/rtrt-mcp")
        );
        assert!(parsed.get("plugin").is_none());
    }

    /// A config with a `//` comment can't be safely round-tripped through
    /// `serde_json`, so this exercises the textual-insert fallback: the
    /// comment must survive, `mcp.other` must survive, and a second apply
    /// must not duplicate the `"rtrt"` key.
    #[test]
    fn opencode_apply_preserves_comments_in_jsonc_fallback_and_stays_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode.jsonc");
        std::fs::write(
            &path,
            "{\n  // keep me\n  \"mcp\": {\n    \"other\": { \"type\": \"local\", \"command\": [\"x\"], },\n  },\n  \"foreign\": true,\n}\n",
        )
        .expect("write starting jsonc");

        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &None)
            .expect("first apply should succeed");
        let first = std::fs::read_to_string(&path).expect("read after first apply");
        assert!(first.contains("// keep me"), "comment must survive");
        assert!(
            first.contains("\"foreign\": true,"),
            "trailing comma must survive"
        );
        assert!(first.contains("\"other\""), "mcp.other must survive");
        assert!(first.contains("\"rtrt\""), "mcp.rtrt must be inserted");
        assert!(!first.contains("rtrt-provenance.js"));

        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &None)
            .expect("second apply should succeed");
        let second = std::fs::read_to_string(&path).expect("read after second apply");
        assert_eq!(
            first, second,
            "re-running apply on a JSONC file must not duplicate the rtrt block"
        );
    }

    #[test]
    fn opencode_uninstall_drops_rtrt_but_keeps_other_server_and_unrelated_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode.jsonc");
        let plugin_url = opencode_provenance_plugin_url(&path).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                // A user- or other-tool-owned key that happens to share a
                // name rtrt once wrote; uninstall must never touch keys it
                // doesn't itself own beyond `mcp.rtrt`.
                "default_agent": "some-other-agent",
                "mcp": {
                    "other": { "type": "local", "command": ["other-server"] },
                    "rtrt": { "type": "local", "command": ["rtrt-mcp"], "enabled": true }
                },
                "plugin": [plugin_url]
            }))
            .unwrap(),
        )
        .expect("write starting config");

        drop_opencode_jsonc_at(&path, true).expect("uninstall should succeed");

        let raw = std::fs::read_to_string(&path).expect("read after drop");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("must be valid JSON");
        assert!(parsed.get("mcp").and_then(|m| m.get("rtrt")).is_none());
        assert!(parsed.get("mcp").and_then(|m| m.get("other")).is_some());
        assert_eq!(parsed["plugin"], serde_json::json!([]));
        assert_eq!(
            parsed.get("default_agent").and_then(|v| v.as_str()),
            Some("some-other-agent")
        );
    }

    #[test]
    fn opencode_uninstall_removes_jsonc_rtrt_surgically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.jsonc");
        std::fs::write(
            &path,
            r#"{
  // keep this comment and trailing commas
  "mcp": {
    "other": {"type": "local", "command": ["other"],},
    "rtrt": {"type": "local", "command": ["rtrt"], "enabled": true,},
  },
  "foreign": true,
}
"#,
        )
        .unwrap();

        drop_opencode_jsonc_at(&path, true).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("// keep this comment and trailing commas"));
        assert!(raw.contains("\"other\""));
        let parsed = parse_json_or_jsonc(&raw, &path).unwrap();
        assert!(parsed["mcp"].get("rtrt").is_none());
        assert_eq!(parsed["mcp"]["other"]["command"][0], "other");
        assert_eq!(parsed["foreign"], true);
    }

    #[test]
    fn opencode_migration_preserves_foreign_plugins_order_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        std::fs::write(&path, r#"{"plugin":["foreign-a",["foreign-b",{"x":1}]]}"#).unwrap();

        apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        let root: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            root["plugin"],
            serde_json::json!([
                "foreign-a",
                ["foreign-b", {"x": 1}]
            ])
        );
    }

    #[test]
    fn opencode_migration_removes_only_legacy_relative_and_absolute_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let plugin_url = opencode_provenance_plugin_url(&path).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({
                "plugin": [
                    "foreign-a",
                    OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID,
                    plugin_url,
                    "foreign-b",
                    OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID,
                    "./plugins/rtrt-provenance.js?foreign"
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["plugin"],
            serde_json::json!([
                "foreign-a",
                "foreign-b",
                "./plugins/rtrt-provenance.js?foreign"
            ])
        );
    }

    #[test]
    fn opencode_config_relocation_selects_corresponding_plugin_url() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first/opencode.json");
        let second = dir.path().join("second/opencode.json");
        assert_ne!(
            opencode_provenance_plugin_url(&first).unwrap(),
            opencode_provenance_plugin_url(&second).unwrap()
        );
        assert!(
            opencode_provenance_plugin_url(&second)
                .unwrap()
                .ends_with("/second/plugins/rtrt-provenance.js")
        );
    }

    #[test]
    fn opencode_migration_preserves_unrelated_non_array_plugin_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.jsonc");
        let original = r#"{"plugin":"foreign","mcp":{"other":{}}}"#;
        std::fs::write(&path, original).unwrap();

        apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["plugin"], "foreign");
        assert!(root["mcp"].get("rtrt").is_some());
    }

    #[test]
    fn opencode_registration_dry_run_does_not_create_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        apply_opencode_jsonc_at(&path, false, "/bin/rtrt-mcp", &None).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn opencode_uninstall_removes_only_exact_provenance_registration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let plugin_url = opencode_provenance_plugin_url(&path).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({
                "plugin": [
                    "foreign-a",
                    plugin_url,
                    OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID,
                    "./plugins/rtrt-provenance.js?foreign",
                    [OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID, {"foreign_tuple": true}],
                    "foreign-b"
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        drop_opencode_jsonc_at(&path, true).unwrap();
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["plugin"],
            serde_json::json!([
                "foreign-a",
                "./plugins/rtrt-provenance.js?foreign",
                [OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID, {"foreign_tuple": true}],
                "foreign-b"
            ])
        );
    }

    #[test]
    fn resolve_opencode_tui_config_path_prefers_json_then_jsonc() {
        let dir = tempfile::tempdir().unwrap();
        let opencode = dir.path().join(".config/opencode");
        std::fs::create_dir_all(&opencode).unwrap();

        assert_eq!(
            resolve_opencode_tui_config_path_in(dir.path()),
            opencode.join("tui.json")
        );
        std::fs::write(opencode.join("tui.jsonc"), "{}").unwrap();
        assert_eq!(
            resolve_opencode_tui_config_path_in(dir.path()),
            opencode.join("tui.jsonc")
        );
        std::fs::write(opencode.join("tui.json"), "{}").unwrap();
        assert_eq!(
            resolve_opencode_tui_config_path_in(dir.path()),
            opencode.join("tui.json")
        );
    }

    #[test]
    fn opencode_tui_strict_json_upsert_is_idempotent_and_preserves_foreign_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.json");
        let foreign_string = serde_json::json!("foreign-package");
        let foreign_tuple = serde_json::json!(["./tui/foreign.tsx", {"theme": "custom"}]);
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "$schema": "https://example.invalid/tui.schema.json",
                "unknown": {"nested": [1, 2, 3]},
                "plugin": [
                    foreign_string.clone(),
                    [OPENCODE_TUI_STATUSLINE_PLUGIN_ID, {"bin": "/old/rtrt", "color": "violet"}],
                    foreign_tuple.clone()
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let binary = Path::new("/opt/RTRT tools/bin/rtrt");

        apply_opencode_tui_config_at(&path, true, binary).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        apply_opencode_tui_config_at(&path, true, binary).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);

        let root: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(root["unknown"], serde_json::json!({"nested": [1, 2, 3]}));
        let plugins = root["plugin"].as_array().unwrap();
        assert_eq!(plugins[0], foreign_string);
        assert_eq!(plugins[2], foreign_tuple);
        assert_eq!(
            plugins
                .iter()
                .filter(|spec| opencode_tui_plugin_spec_id(spec)
                    == Some(OPENCODE_TUI_STATUSLINE_PLUGIN_ID))
                .count(),
            1
        );
        assert_eq!(plugins[1][1]["bin"], binary.to_string_lossy().as_ref());
        assert_eq!(plugins[1][1]["color"], "violet");
        assert!(root.get("_rtrt").is_none());
        assert!(plugins[1][1].get("_rtrt").is_none());
    }

    #[test]
    fn opencode_tui_jsonc_merge_and_uninstall_are_surgical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.jsonc");
        std::fs::write(
            &path,
            r#"{
  // JSONC comment
  "unknown": {"keep": true,},
  "plugin": [
    "foreign-package",
    ["./tui/foreign.tsx", {"option": 7,}],
  ],
}
"#,
        )
        .unwrap();
        let binary = Path::new("/home/user/My Tools/rtrt");

        apply_opencode_tui_config_at(&path, true, binary).unwrap();
        let installed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(installed["unknown"]["keep"], true);
        assert_eq!(installed["plugin"].as_array().unwrap().len(), 3);
        assert_eq!(installed["plugin"][2][0], OPENCODE_TUI_STATUSLINE_PLUGIN_ID);
        assert_eq!(installed["plugin"][2][1]["bin"], "/home/user/My Tools/rtrt");

        drop_opencode_tui_config_at(&path, true).unwrap();
        let removed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(removed["unknown"]["keep"], true);
        assert_eq!(
            removed["plugin"],
            serde_json::json!([
                "foreign-package",
                ["./tui/foreign.tsx", {"option": 7}]
            ])
        );
    }

    #[test]
    fn opencode_tui_marked_old_versions_upgrade_backup_and_repeat() {
        let dir = tempfile::tempdir().unwrap();
        for (name, begin, end, current) in [
            (
                OPENCODE_TUI_STATUSLINE_FILE,
                OPENCODE_TUI_STATUSLINE_BEGIN,
                OPENCODE_TUI_STATUSLINE_END,
                managed_tui_statusline_source(),
            ),
            (
                OPENCODE_TUI_STATUSLINE_CORE_FILE,
                OPENCODE_TUI_STATUSLINE_CORE_BEGIN,
                OPENCODE_TUI_STATUSLINE_CORE_END,
                managed_tui_statusline_core_source(),
            ),
        ] {
            let path = dir.path().join(name);
            let old_v1 = managed_tui_source("old RTRT version 1", begin, end);
            let old_v2 = managed_tui_source("old RTRT version 2", begin, end);
            std::fs::write(&path, &old_v1).unwrap();
            std::fs::write(backup_path(&path), "stale backup").unwrap();

            install_managed_tui_file_at(&path, &current, begin, end, true).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
            assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), old_v1);

            install_managed_tui_file_at(&path, &current, begin, end, true).unwrap();
            assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), old_v1);

            std::fs::write(&path, &old_v2).unwrap();
            install_managed_tui_file_at(&path, &current, begin, end, true).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
            assert_eq!(std::fs::read_to_string(backup_path(&path)).unwrap(), old_v2);
        }
    }

    #[test]
    fn opencode_tui_managed_files_refuse_outside_marker_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OPENCODE_TUI_STATUSLINE_FILE);
        let old = managed_tui_source(
            "old RTRT version",
            OPENCODE_TUI_STATUSLINE_BEGIN,
            OPENCODE_TUI_STATUSLINE_END,
        );
        let edited = format!("{old}// user-owned suffix\n");
        std::fs::write(&path, &edited).unwrap();

        let error = install_managed_tui_file_at(
            &path,
            &managed_tui_statusline_source(),
            OPENCODE_TUI_STATUSLINE_BEGIN,
            OPENCODE_TUI_STATUSLINE_END,
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn opencode_tui_managed_files_refuse_unknown_and_removed_markers() {
        let dir = tempfile::tempdir().unwrap();
        let unknown_path = dir.path().join("unknown.tsx");
        std::fs::write(&unknown_path, "export const userOwned = true\n").unwrap();
        let current = managed_tui_statusline_source();

        let error = install_managed_tui_file_at(
            &unknown_path,
            &current,
            OPENCODE_TUI_STATUSLINE_BEGIN,
            OPENCODE_TUI_STATUSLINE_END,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            std::fs::read_to_string(&unknown_path).unwrap(),
            "export const userOwned = true\n"
        );
        assert!(!backup_path(&unknown_path).exists());

        let edited_path = dir.path().join(OPENCODE_TUI_STATUSLINE_FILE);
        let marker_stripped =
            current.replacen(&format!("{OPENCODE_TUI_STATUSLINE_BEGIN}\n"), "", 1);
        std::fs::write(&edited_path, &marker_stripped).unwrap();
        let error = install_managed_tui_file_at(
            &edited_path,
            &current,
            OPENCODE_TUI_STATUSLINE_BEGIN,
            OPENCODE_TUI_STATUSLINE_END,
            true,
        )
        .unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        remove_managed_tui_file_at(
            &edited_path,
            &current,
            OPENCODE_TUI_STATUSLINE_BEGIN,
            OPENCODE_TUI_STATUSLINE_END,
            true,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&edited_path).unwrap(),
            marker_stripped
        );
    }

    #[test]
    fn opencode_tui_install_and_uninstall_manage_only_owned_files_and_tuple() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("config with spaces/tui");
        let config = dir.path().join("config with spaces/tui.json");
        let binary = Path::new("/opt/RTRT tools/rtrt");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            serde_json::to_string_pretty(&serde_json::json!({
                "plugin": ["foreign-plugin"],
                "foreign": "keep"
            }))
            .unwrap(),
        )
        .unwrap();

        install_opencode_tui_statusline_at(&root, &config, true, binary).unwrap();
        let statusline_path = root.join(OPENCODE_TUI_STATUSLINE_FILE);
        let core_path = root.join(OPENCODE_TUI_STATUSLINE_CORE_FILE);
        assert_eq!(
            std::fs::read_to_string(&statusline_path).unwrap(),
            managed_tui_statusline_source()
        );
        assert_eq!(
            std::fs::read_to_string(&core_path).unwrap(),
            managed_tui_statusline_core_source()
        );

        std::fs::write(
            &statusline_path,
            format!(
                "{}export const userEdit = true\n",
                managed_tui_statusline_source()
            ),
        )
        .unwrap();
        let edited_statusline = std::fs::read_to_string(&statusline_path).unwrap();
        remove_opencode_tui_statusline_at(&root, &config, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(&statusline_path).unwrap(),
            edited_statusline
        );
        assert!(!core_path.exists());
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(root["plugin"], serde_json::json!(["foreign-plugin"]));
        assert_eq!(root["foreign"], "keep");
    }
}
