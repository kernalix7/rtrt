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
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rtrt_core::OutputStyleLevel;

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
const OPENCODE_NPM_PLUGIN_NAME: &str = "rtrt-agent";
const OPENCODE_NPM_PLUGIN_ID: &str = concat!("rtrt-agent@", env!("CARGO_PKG_VERSION"));
/// Relative registration emitted by older RTRT versions. OpenCode resolves it
/// against the current package/project, not the global config directory.
const OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID: &str = "./plugins/rtrt-provenance.js";
const OPENCODE_PROVENANCE_STATE_FILE: &str = ".rtrt-provenance-state.json";
const OPENCODE_PROVENANCE_STATE_OWNER: &str = "rtrt-opencode-provenance-plugin";
const OPENCODE_PROVENANCE_STATE_VERSION: u64 = 1;
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
];

// Exact former setup-installed bytes; retained only to recognize and retire the owned legacy file.
const CLAUDE_TECH_LEAD_LEGACY: AgentSpec = AgentSpec {
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
};

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
    Rules,
    TuiStatusline,
    McpConfig,
    ProvenancePlugin,
}

fn run_opencode_setup_steps(mut run: impl FnMut(OpenCodeSetupSurface) -> Result<()>) -> Result<()> {
    for surface in [
        OpenCodeSetupSurface::Rules,
        OpenCodeSetupSurface::TuiStatusline,
        OpenCodeSetupSurface::McpConfig,
        OpenCodeSetupSurface::ProvenancePlugin,
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
    } else if plan.no_sandbox {
        let boundary = crate::sandbox::discover_project()?;
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
            let provenance_path =
                opencode_provenance_plugin_path_in(&resolve_opencode_config_root()?);
            inspect_opencode_provenance_plugin_at(&provenance_path, false)?;
            run_opencode_setup_steps(|surface| match surface {
                OpenCodeSetupSurface::ProvenancePlugin => {
                    remove_opencode_provenance_plugin_at_with_policy(
                        &provenance_path,
                        plan.apply,
                        false,
                    )
                }
                OpenCodeSetupSurface::Rules => install_opencode_agents_rules(plan.apply),
                OpenCodeSetupSurface::TuiStatusline => install_opencode_tui_statusline(plan.apply),
                OpenCodeSetupSurface::McpConfig => {
                    apply_opencode_jsonc(&plan, &binary, &memory_path)
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
        Some(p) => serde_json::json!(["--admin", "--memory", p.to_string_lossy()]),
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
                "args = [\"--admin\", \"--memory\", {:?}]\n",
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
        command.push(serde_json::Value::String("--admin".to_string()));
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
    let root = resolve_opencode_config_root()?;
    Ok(resolve_opencode_config_path_in(&root))
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

fn resolve_opencode_config_path_in(root: &Path) -> PathBuf {
    let json = root.join("opencode.json");
    if json.exists() {
        return json;
    }
    let jsonc = root.join("opencode.jsonc");
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
            "plugin": [OPENCODE_NPM_PLUGIN_ID],
        });
        let rendered = serde_json::to_string_pretty(&root)?;
        write_private_file_atomic_same_dir(path, rendered.as_bytes())?;
        println!(
            "wrote {} with mcp.rtrt and the {OPENCODE_NPM_PLUGIN_ID} plugin",
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
    upsert_opencode_plugin_registration(&mut root, path, &plugin_url)?;
    if root != before {
        write_opencode_config(path, &raw, &before, &root)?;
    }
    println!(
        "merged mcp.rtrt and {OPENCODE_NPM_PLUGIN_ID} into {}",
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
        "[dry-run] would register {OPENCODE_NPM_PLUGIN_ID} and migrate legacy registrations {plugin_url} and {OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID}"
    )
}

fn is_rtrt_opencode_plugin_string(plugin: &serde_json::Value, plugin_url: &str) -> bool {
    plugin.as_str().is_some_and(|id| {
        is_rtrt_opencode_package(id) || is_exact_legacy_opencode_plugin(id, plugin_url)
    }) || is_rtrt_opencode_package_tuple(plugin)
        || is_rtrt_opencode_package_object(plugin)
}

fn is_rtrt_opencode_package(id: &str) -> bool {
    if id == OPENCODE_NPM_PLUGIN_NAME {
        return true;
    }
    id.strip_prefix("rtrt-agent@")
        .is_some_and(|suffix| !suffix.is_empty())
}

fn is_exact_legacy_opencode_plugin(id: &str, plugin_url: &str) -> bool {
    id == OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID || id == plugin_url
}

fn is_rtrt_opencode_package_tuple(plugin: &serde_json::Value) -> bool {
    let Some(tuple) = plugin.as_array() else {
        return false;
    };
    tuple.len() == 2
        && tuple[0].as_str().is_some_and(is_rtrt_opencode_package)
        && tuple[1].is_object()
}

fn is_rtrt_opencode_package_object(plugin: &serde_json::Value) -> bool {
    let Some(object) = plugin.as_object() else {
        return false;
    };
    if object.len() > 2
        || !object
            .keys()
            .all(|key| matches!(key.as_str(), "package" | "options"))
    {
        return false;
    }
    object
        .get("package")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_rtrt_opencode_package)
        && object
            .get("options")
            .is_none_or(serde_json::Value::is_object)
}

fn upsert_opencode_plugin_registration(
    root: &mut serde_json::Value,
    path: &Path,
    plugin_url: &str,
) -> Result<bool> {
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: root is not a JSON object", path.display()))?;
    let plugins = object
        .entry("plugin")
        .or_insert_with(|| serde_json::json!([]));
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: plugin is not an array", path.display()))?;
    let before = plugins.clone();
    let mut registered = false;
    let mut merged = Vec::with_capacity(before.len() + 1);
    for plugin in &before {
        if is_rtrt_opencode_plugin_string(plugin, plugin_url) {
            if !registered {
                merged.push(serde_json::Value::String(
                    OPENCODE_NPM_PLUGIN_ID.to_string(),
                ));
                registered = true;
            }
        } else {
            merged.push(plugin.clone());
        }
    }
    if !registered {
        merged.push(serde_json::Value::String(
            OPENCODE_NPM_PLUGIN_ID.to_string(),
        ));
    }
    let changed = merged != before;
    *plugins = merged;
    Ok(changed)
}

fn drop_opencode_plugin_registration(
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
    let plugins = plugins
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: plugin is not an array", path.display()))?;
    let before = plugins.len();
    plugins.retain(|plugin| !is_rtrt_opencode_plugin_string(plugin, plugin_url));
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
            "[dry-run] would unset mcp.rtrt and remove {OPENCODE_NPM_PLUGIN_ID} plus legacy registrations {plugin_url} and {OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID} in {}",
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
    removed |= drop_opencode_plugin_registration(&mut root, path, &plugin_url)?;
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

fn resolve_opencode_tui_config_path_in(root: &Path) -> PathBuf {
    let json = root.join("tui.json");
    if json.exists() {
        return json;
    }
    let jsonc = root.join("tui.jsonc");
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
    reject_symlink(path, "OpenCode TUI managed file")?;
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
    reject_symlink(path, "OpenCode TUI managed file")?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        write_private_file_atomic_same_dir(path, current.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
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
    reject_symlink(&backup, "OpenCode TUI backup")?;
    write_private_file_atomic_same_dir(&backup, raw.as_bytes())
        .with_context(|| format!("backup {}", backup.display()))?;
    write_private_file_atomic_same_dir(path, current.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
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
            .context("OpenCode TUI config root is not a JSON object")?
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
    if let Err(error) = write_private_file_atomic_same_dir(path, rendered.as_bytes()) {
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
    let config_root = resolve_opencode_config_root()?;
    let root = opencode_tui_root_in(&config_root);
    let config = resolve_opencode_tui_config_path_in(&config_root);
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
    let config_root = resolve_opencode_config_root()?;
    let root = opencode_tui_root_in(&config_root);
    let config = resolve_opencode_tui_config_path_in(&config_root);
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

fn resolve_opencode_config_root() -> Result<PathBuf> {
    let opencode = std::env::var_os("OPENCODE_CONFIG_DIR").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    resolve_opencode_config_root_from(
        opencode.as_deref(),
        xdg.as_deref(),
        home.as_deref(),
        profile.as_deref(),
    )
}

fn resolve_opencode_config_root_from(
    opencode: Option<&Path>,
    xdg: Option<&Path>,
    home: Option<&Path>,
    profile: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(root) = opencode.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(root.to_path_buf());
    }
    if let Some(root) = xdg.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(root.join("opencode"));
    }
    if let Some(root) = home
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| profile.filter(|path| !path.as_os_str().is_empty()))
    {
        return Ok(root.join(".config/opencode"));
    }
    bail!("cannot resolve OpenCode config root: no nonempty config or home environment is set")
}

fn opencode_rules_path_in(root: &Path) -> PathBuf {
    root.join("AGENTS.md")
}

fn opencode_provenance_plugin_path_in(root: &Path) -> PathBuf {
    root.join("plugins/rtrt-provenance.js")
}

fn opencode_tui_root_in(root: &Path) -> PathBuf {
    root.join("tui")
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

fn retire_legacy_claude_tech_lead_at(agents_root: &Path, apply: bool) -> Result<()> {
    let path = claude_agent_path(agents_root, &CLAUDE_TECH_LEAD_LEGACY);
    let Some(existing) = read_real_file(&path, "legacy Claude tech-lead agent")? else {
        return Ok(());
    };
    if existing.as_bytes() != CLAUDE_TECH_LEAD_LEGACY.body.as_bytes() {
        return Ok(());
    }
    if !apply {
        println!(
            "[dry-run] would back up and remove legacy Claude agent file {}",
            path.display()
        );
        return Ok(());
    }

    let backup = backup_path(&path);
    match read_real_file(&backup, "legacy Claude tech-lead backup")? {
        Some(content) if content.as_bytes() == CLAUDE_TECH_LEAD_LEGACY.body.as_bytes() => {}
        Some(_) => bail!(
            "{}: conflicting legacy Claude tech-lead backup; refusing to remove source",
            backup.display()
        ),
        None => {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&backup);
            let mut file = match file {
                Ok(file) => Some(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create backup {}", backup.display()));
                }
            };
            if let Some(file) = file.as_mut() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))
                        .with_context(|| format!("chmod 0600 {}", backup.display()))?;
                }
                file.write_all(CLAUDE_TECH_LEAD_LEGACY.body.as_bytes())
                    .with_context(|| format!("write backup {}", backup.display()))?;
                file.sync_all()
                    .with_context(|| format!("sync backup {}", backup.display()))?;
            }
            let content = read_real_file(&backup, "legacy Claude tech-lead backup")?
                .ok_or_else(|| anyhow::anyhow!("{}: backup disappeared", backup.display()))?;
            if content.as_bytes() != CLAUDE_TECH_LEAD_LEGACY.body.as_bytes() {
                bail!(
                    "{}: conflicting legacy Claude tech-lead backup; refusing to remove source",
                    backup.display()
                );
            }
        }
    }

    let backup_content = read_real_file(&backup, "legacy Claude tech-lead backup")?
        .ok_or_else(|| anyhow::anyhow!("{}: backup disappeared", backup.display()))?;
    if backup_content.as_bytes() != CLAUDE_TECH_LEAD_LEGACY.body.as_bytes() {
        bail!(
            "{}: backup changed during retirement; refusing to remove source",
            backup.display()
        );
    }

    let current = read_real_file(&path, "legacy Claude tech-lead agent")?
        .ok_or_else(|| anyhow::anyhow!("{}: source disappeared", path.display()))?;
    if current.as_bytes() != CLAUDE_TECH_LEAD_LEGACY.body.as_bytes() {
        bail!("{}: source changed during retirement", path.display());
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    println!("retired legacy Claude agent file {}", path.display());
    Ok(())
}

fn install_claude_skills_agents(apply: bool) -> Result<()> {
    let skills_root = expand_home(CLAUDE_SKILLS_ROOT_REL)?;
    let agents_root = expand_home(CLAUDE_AGENTS_ROOT_REL)?;
    retire_legacy_claude_tech_lead_at(&agents_root, apply)?;
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
        AgentKind::Opencode => None,
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

fn set_private_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
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

fn install_opencode_agents_rules(apply: bool) -> Result<()> {
    let path = opencode_rules_path_in(&resolve_opencode_config_root()?);
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
    reject_symlink(path, "OpenCode rules")?;
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
    write_private_file_atomic_same_dir(path, rendered.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
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

#[cfg(test)]
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

#[cfg(test)]
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
    let path = opencode_provenance_plugin_path_in(&resolve_opencode_config_root()?);
    remove_opencode_provenance_plugin_at(&path, apply)
}

fn remove_opencode_provenance_plugin_at(path: &Path, apply: bool) -> Result<()> {
    remove_opencode_provenance_plugin_at_with_policy(path, apply, true)
}

#[derive(Debug, PartialEq, Eq)]
enum OpenCodeProvenanceBackupInspection {
    Discard(PathBuf),
    Foreign { path: PathBuf, content: String },
}

#[derive(Debug, PartialEq, Eq)]
enum OpenCodeProvenanceInspection {
    NoPlugin,
    RemoveOrphanState(PathBuf),
    PreserveUnrecognized,
    RemoveManaged {
        state_path: Option<PathBuf>,
        backup: Option<OpenCodeProvenanceBackupInspection>,
    },
}

fn inspect_opencode_provenance_plugin_at(
    path: &Path,
    preserve_unrecognized: bool,
) -> Result<OpenCodeProvenanceInspection> {
    let state_path = opencode_provenance_state_path(path)?;
    let state_content = load_opencode_provenance_state(&state_path, path)?;
    let Some(raw) = read_real_file(path, "provenance plugin")? else {
        return Ok(match state_content {
            Some(_) => OpenCodeProvenanceInspection::RemoveOrphanState(state_path),
            None => OpenCodeProvenanceInspection::NoPlugin,
        });
    };
    let managed = managed_opencode_provenance_plugin();
    let recognized = match state_content.as_deref() {
        Some(installed) => raw == installed,
        None => {
            raw == managed
                || raw == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1
                || raw == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2
        }
    };
    if !recognized {
        if preserve_unrecognized {
            return Ok(OpenCodeProvenanceInspection::PreserveUnrecognized);
        }
        bail!(
            "{}: plugin content was modified or is not rtrt-managed; refusing to remove it",
            path.display()
        );
    }

    let backup_path = backup_path(path);
    let backup = match read_real_file(&backup_path, "provenance backup")? {
        Some(content)
            if content.is_empty()
                || content == managed
                || content == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1
                || content == OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2 =>
        {
            Some(OpenCodeProvenanceBackupInspection::Discard(backup_path))
        }
        Some(content) => Some(OpenCodeProvenanceBackupInspection::Foreign {
            path: backup_path,
            content,
        }),
        None => None,
    };
    Ok(OpenCodeProvenanceInspection::RemoveManaged {
        state_path: state_content.map(|_| state_path),
        backup,
    })
}

fn remove_opencode_provenance_plugin_at_with_policy(
    path: &Path,
    apply: bool,
    preserve_unrecognized: bool,
) -> Result<()> {
    let inspection = inspect_opencode_provenance_plugin_at(path, preserve_unrecognized)?;
    if !apply {
        println!("[dry-run] would remove {}", path.display());
        return Ok(());
    }
    match inspection {
        OpenCodeProvenanceInspection::NoPlugin => Ok(()),
        OpenCodeProvenanceInspection::RemoveOrphanState(state_path) => {
            std::fs::remove_file(&state_path)
                .with_context(|| format!("remove {}", state_path.display()))?;
            Ok(())
        }
        OpenCodeProvenanceInspection::PreserveUnrecognized => {
            println!(
                "{}: plugin content was modified or is not rtrt-managed; preserving it",
                path.display()
            );
            Ok(())
        }
        OpenCodeProvenanceInspection::RemoveManaged { state_path, backup } => {
            match &backup {
                Some(OpenCodeProvenanceBackupInspection::Foreign { content, .. }) => {
                    write_private_file_atomic_same_dir(path, content.as_bytes())
                        .with_context(|| format!("write {}", path.display()))?;
                }
                Some(OpenCodeProvenanceBackupInspection::Discard(_)) | None => {
                    std::fs::remove_file(path)
                        .with_context(|| format!("remove {}", path.display()))?;
                }
            }
            match backup {
                Some(OpenCodeProvenanceBackupInspection::Discard(backup_path))
                | Some(OpenCodeProvenanceBackupInspection::Foreign {
                    path: backup_path, ..
                }) => {
                    std::fs::remove_file(&backup_path)
                        .with_context(|| format!("remove {}", backup_path.display()))?;
                }
                None => {}
            }
            if let Some(state_path) = state_path {
                std::fs::remove_file(&state_path)
                    .with_context(|| format!("remove {}", state_path.display()))?;
            }
            println!(
                "removed managed OpenCode provenance plugin from {}",
                path.display()
            );
            Ok(())
        }
    }
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
    let path = opencode_rules_path_in(&resolve_opencode_config_root()?);
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
    retire_legacy_claude_tech_lead_at(&agents_root, apply)?;
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
    Rules,
    ProvenancePlugin,
    TuiStatusline,
    McpConfig,
}

impl OpenCodeUninstallSurface {
    fn label(self) -> &'static str {
        match self {
            Self::Sandbox => "shell sandbox",
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
        OpenCodeUninstallSurface::Rules,
        // Unregister before deleting the file so an interrupted uninstall
        // never leaves OpenCode pointing at a missing RTRT plugin.
        OpenCodeUninstallSurface::McpConfig,
        OpenCodeUninstallSurface::ProvenancePlugin,
        OpenCodeUninstallSurface::TuiStatusline,
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
    let config_path = resolve_opencode_config_path()?;
    run_opencode_uninstall_steps(|surface| match surface {
        OpenCodeUninstallSurface::Sandbox => disable_opencode_sandbox_global(apply),
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
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    {
        let file = temporary.as_file_mut();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 temporary file in {}", parent.display()))?;
        }
        file.write_all(contents)
            .with_context(|| format!("write temporary file in {}", parent.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temporary file in {}", parent.display()))?;
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace {}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", parent.display()))?;
    Ok(())
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

pub fn claude_settings_status(health: bool) -> ClaudeSettingsStatus {
    let missing = ClaudeSettingsStatus {
        hooks_state: CheckState::Warn,
        hooks_detail: "settings file missing".into(),
        statusline_state: CheckState::Warn,
        statusline_detail: "settings file missing".into(),
    };
    let Some(path) = dirs_home()
        .ok()
        .map(|home| home.join(".claude/settings.json"))
    else {
        return ClaudeSettingsStatus {
            hooks_state: CheckState::Warn,
            hooks_detail: "home directory unavailable".into(),
            statusline_state: CheckState::Warn,
            statusline_detail: "home directory unavailable".into(),
        };
    };
    if !path.exists() {
        return missing;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            let detail = format!("read failed: {error}");
            let state = if health {
                CheckState::Fail
            } else {
                CheckState::Warn
            };
            return ClaudeSettingsStatus {
                hooks_state: state,
                hooks_detail: detail.clone(),
                statusline_state: state,
                statusline_detail: detail,
            };
        }
    };
    let root: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(root) => root,
        Err(error) => {
            let detail = format!("invalid JSON: {error}");
            let state = if health {
                CheckState::Fail
            } else {
                CheckState::Warn
            };
            return ClaudeSettingsStatus {
                hooks_state: state,
                hooks_detail: detail.clone(),
                statusline_state: state,
                statusline_detail: detail,
            };
        }
    };
    let hooks_present = root
        .get("hooks")
        .is_some_and(|hooks| json_contains_text(hooks, CLAUDE_HOOK_NEEDLE));
    let statusline_present = root
        .get("statusLine")
        .is_some_and(|line| json_contains_text(line, CLAUDE_STATUSLINE_NEEDLE));
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

    #[test]
    fn legacy_claude_tech_lead_is_not_an_active_agent() {
        // Given: the active Claude agent inventory.
        // When: its machine-consumed agent names are inspected.
        // Then: the retired legacy asset is absent.
        assert!(
            CLAUDE_AGENTS
                .iter()
                .all(|agent| agent.name != CLAUDE_TECH_LEAD_LEGACY.name)
        );
    }

    #[test]
    fn legacy_claude_tech_lead_retirement_backs_up_before_removal_and_is_idempotent() {
        // Given: the exact setup-installed legacy agent and no backup.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        std::fs::write(&path, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();

        // When: retirement is applied twice.
        retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap();
        retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap();

        // Then: the source is gone and its exact former bytes remain backed up.
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(backup_path(&path)).unwrap(),
            CLAUDE_TECH_LEAD_LEGACY.body.as_bytes()
        );
    }

    #[test]
    fn legacy_claude_tech_lead_retirement_dry_run_has_no_effect() {
        // Given: the exact setup-installed legacy agent.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        std::fs::write(&path, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();

        // When: retirement is only previewed.
        retire_legacy_claude_tech_lead_at(dir.path(), false).unwrap();

        // Then: neither source nor backup changed.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            CLAUDE_TECH_LEAD_LEGACY.body.as_bytes()
        );
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn legacy_claude_tech_lead_retirement_preserves_foreign_file_without_backup() {
        // Given: a user-modified file at the retired agent path.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        std::fs::write(&path, b"user-owned agent\n").unwrap();

        // When: retirement is applied.
        retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap();

        // Then: the foreign file is untouched and no backup is created.
        assert_eq!(std::fs::read(&path).unwrap(), b"user-owned agent\n");
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn legacy_claude_tech_lead_retirement_accepts_identical_backup() {
        // Given: the exact legacy source and an identical prior backup.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        std::fs::write(&path, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();
        std::fs::write(backup_path(&path), CLAUDE_TECH_LEAD_LEGACY.body).unwrap();

        // When: retirement is applied.
        retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap();

        // Then: the owned source is removed and the identical backup remains.
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(backup_path(&path)).unwrap(),
            CLAUDE_TECH_LEAD_LEGACY.body.as_bytes()
        );
    }

    #[test]
    fn legacy_claude_tech_lead_retirement_rejects_conflicting_backup_before_removal() {
        // Given: the exact legacy source and a foreign backup.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        let backup = backup_path(&path);
        std::fs::write(&path, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();
        std::fs::write(&backup, b"foreign backup\n").unwrap();

        // When: retirement is attempted.
        let error = retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap_err();

        // Then: it fails without deleting either file.
        assert!(error.to_string().contains("backup"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            CLAUDE_TECH_LEAD_LEGACY.body.as_bytes()
        );
        assert_eq!(std::fs::read(&backup).unwrap(), b"foreign backup\n");
    }

    #[cfg(unix)]
    #[test]
    fn legacy_claude_tech_lead_retirement_rejects_symlink_before_removal() {
        use std::os::unix::fs::symlink;

        // Given: a symlink at the retired agent path.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.md");
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        std::fs::write(&target, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();
        symlink(&target, &path).unwrap();

        // When: retirement is attempted.
        let error = retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap_err();

        // Then: it fails before deleting the link or creating a backup.
        assert!(error.to_string().contains("real file"));
        assert!(std::fs::symlink_metadata(&path).is_ok());
        assert!(!backup_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_claude_tech_lead_retirement_rejects_backup_symlink_before_removal() {
        use std::os::unix::fs::symlink;

        // Given: the exact legacy source and a symlink at its backup path.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        let backup = backup_path(&path);
        let target = dir.path().join("backup-target.md");
        std::fs::write(&path, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();
        std::fs::write(&target, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();
        symlink(&target, &backup).unwrap();

        // When: retirement is attempted.
        let error = retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap_err();

        // Then: it fails before deleting the owned source.
        assert!(error.to_string().contains("real file"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            CLAUDE_TECH_LEAD_LEGACY.body.as_bytes()
        );
        assert!(std::fs::symlink_metadata(&backup).is_ok());
    }

    #[test]
    fn legacy_claude_tech_lead_retirement_rejects_backup_io_failure_before_removal() {
        // Given: the exact legacy source and a directory blocking its backup path.
        let dir = tempfile::tempdir().unwrap();
        let path = claude_agent_path(dir.path(), &CLAUDE_TECH_LEAD_LEGACY);
        let backup = backup_path(&path);
        std::fs::write(&path, CLAUDE_TECH_LEAD_LEGACY.body).unwrap();
        std::fs::create_dir(&backup).unwrap();

        // When: retirement is attempted.
        let error = retire_legacy_claude_tech_lead_at(dir.path(), true).unwrap_err();

        // Then: it fails before deleting the owned source.
        assert!(error.to_string().contains("real file"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            CLAUDE_TECH_LEAD_LEGACY.body.as_bytes()
        );
        assert!(backup.is_dir());
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
    fn json_entry_explicit_memory_enables_admin_profile() {
        // Given: an explicit legacy/admin memory path.
        let memory_path = Some(PathBuf::from("/home/u/.rtrt/memory.sqlite"));

        // When: a conventional MCP JSON entry is built.
        let entry = build_json_entry("/usr/local/bin/rtrt-mcp", &memory_path);

        // Then: its structured argv selects admin mode before the memory path.
        assert_eq!(
            entry.get("args").and_then(serde_json::Value::as_array),
            Some(&vec![
                serde_json::json!("--admin"),
                serde_json::json!("--memory"),
                serde_json::json!("/home/u/.rtrt/memory.sqlite"),
            ])
        );
    }

    #[test]
    fn codex_snippet_explicit_memory_enables_admin_profile() {
        // Given: an explicit legacy/admin memory path.
        let memory_path = Some(PathBuf::from("/home/u/.rtrt/memory.sqlite"));

        // When: the Codex MCP TOML snippet is rendered and parsed.
        let snippet = render_codex_toml_snippet("/usr/local/bin/rtrt-mcp", &memory_path);
        let parsed: toml::Value = toml::from_str(&snippet).unwrap();

        // Then: its structured argv selects admin mode before the memory path.
        assert_eq!(
            parsed["mcp_servers"]["rtrt"]["args"],
            toml::Value::Array(vec![
                toml::Value::String("--admin".into()),
                toml::Value::String("--memory".into()),
                toml::Value::String("/home/u/.rtrt/memory.sqlite".into()),
            ])
        );
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
                "--admin",
                "--memory",
                "/home/u/.rtrt/memory.sqlite",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn opencode_uninstall_runs_all_surfaces_and_aggregates_errors() {
        let mut visited = Vec::new();
        let error = run_opencode_uninstall_steps(|surface| {
            visited.push(surface);
            if surface == OpenCodeUninstallSurface::Rules {
                bail!("rules failure");
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(visited.len(), 4);
        assert_eq!(visited[0], OpenCodeUninstallSurface::Rules);
        assert_eq!(visited[1], OpenCodeUninstallSurface::McpConfig);
        assert_eq!(visited[2], OpenCodeUninstallSurface::ProvenancePlugin);
        assert!(!visited.contains(&OpenCodeUninstallSurface::Sandbox));
        assert!(error.to_string().contains("2 error(s)"));
        assert!(error.to_string().contains("rules: rules failure"));
        assert!(error.to_string().contains("sandbox: retained"));
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
    fn opencode_setup_runs_legacy_cleanup_last_and_skips_it_after_failure() {
        let mut visited = Vec::new();
        run_opencode_setup_steps(|surface| {
            visited.push(surface);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            visited,
            vec![
                OpenCodeSetupSurface::Rules,
                OpenCodeSetupSurface::TuiStatusline,
                OpenCodeSetupSurface::McpConfig,
                OpenCodeSetupSurface::ProvenancePlugin,
            ]
        );

        visited.clear();
        let error = run_opencode_setup_steps(|surface| {
            visited.push(surface);
            if surface == OpenCodeSetupSurface::McpConfig {
                bail!("registration failure");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("registration failure"));
        assert_eq!(
            visited,
            vec![
                OpenCodeSetupSurface::Rules,
                OpenCodeSetupSurface::TuiStatusline,
                OpenCodeSetupSurface::McpConfig,
            ]
        );
    }

    #[test]
    fn opencode_setup_preserves_legacy_runtime_when_registration_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("opencode.json");
        let plugin = dir.path().join("plugins/rtrt-provenance.js");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2).unwrap();
        std::fs::write(&config, r#"{"plugin":"foreign"}"#).unwrap();

        let error = run_opencode_setup_steps(|surface| match surface {
            OpenCodeSetupSurface::Rules | OpenCodeSetupSurface::TuiStatusline => Ok(()),
            OpenCodeSetupSurface::McpConfig => {
                apply_opencode_jsonc_at(&config, true, "/bin/rtrt-mcp", &None)
            }
            OpenCodeSetupSurface::ProvenancePlugin => {
                remove_opencode_provenance_plugin_at_with_policy(&plugin, true, false)
            }
        })
        .unwrap_err();

        assert!(error.to_string().contains("plugin is not an array"));
        assert_eq!(
            std::fs::read_to_string(&plugin).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2
        );
    }

    #[test]
    fn opencode_setup_rejects_internal_provenance_edit_before_any_surface_runs() {
        // Given
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("opencode.json");
        let plugin = dir.path().join("plugins/rtrt-provenance.js");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        let original_config = b"{}\n";
        let mut modified_plugin = OPENCODE_PROVENANCE_PLUGIN.as_bytes().to_vec();
        let edited_byte = OPENCODE_PROVENANCE_PLUGIN
            .find("const RTRT_AGENT_TOOLS")
            .unwrap();
        modified_plugin[edited_byte] = b'C';
        std::fs::write(&config, original_config).unwrap();
        std::fs::write(&plugin, &modified_plugin).unwrap();
        let mut setup_surface_invoked = false;

        // When
        let error = (|| {
            remove_opencode_provenance_plugin_at_with_policy(&plugin, false, false)?;
            run_opencode_setup_steps(|surface| {
                setup_surface_invoked = true;
                match surface {
                    OpenCodeSetupSurface::Rules | OpenCodeSetupSurface::TuiStatusline => Ok(()),
                    OpenCodeSetupSurface::McpConfig => {
                        apply_opencode_jsonc_at(&config, true, "/bin/rtrt-mcp", &None)
                    }
                    OpenCodeSetupSurface::ProvenancePlugin => {
                        remove_opencode_provenance_plugin_at_with_policy(&plugin, true, false)
                    }
                }
            })
        })()
        .unwrap_err();

        // Then
        assert!(error.to_string().contains("refusing to remove"));
        assert!(!setup_surface_invoked);
        assert_eq!(std::fs::read(&config).unwrap(), original_config);
        assert_eq!(std::fs::read(&plugin).unwrap(), modified_plugin);
        assert!(!backup_path(&plugin).exists());
    }

    #[test]
    fn opencode_setup_leaves_one_runtime_after_migrating_managed_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("opencode.json");
        let plugin = dir.path().join("plugins/rtrt-provenance.js");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2).unwrap();

        run_opencode_setup_steps(|surface| match surface {
            OpenCodeSetupSurface::Rules | OpenCodeSetupSurface::TuiStatusline => Ok(()),
            OpenCodeSetupSurface::McpConfig => {
                apply_opencode_jsonc_at(&config, true, "/bin/rtrt-mcp", &None)
            }
            OpenCodeSetupSurface::ProvenancePlugin => {
                remove_opencode_provenance_plugin_at_with_policy(&plugin, true, false)
            }
        })
        .unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(root["plugin"], serde_json::json!([OPENCODE_NPM_PLUGIN_ID]));
        assert!(!plugin.exists());
    }

    #[test]
    fn opencode_setup_cleanup_fails_safely_for_modified_provenance_file() {
        let dir = tempfile::tempdir().unwrap();
        let plugin = dir.path().join("rtrt-provenance.js");
        let modified = format!("{OPENCODE_PROVENANCE_PLUGIN}export const UserAddition = true\n");
        std::fs::write(&plugin, &modified).unwrap();

        let error =
            remove_opencode_provenance_plugin_at_with_policy(&plugin, true, false).unwrap_err();

        assert!(error.to_string().contains("refusing to remove"));
        assert_eq!(std::fs::read_to_string(&plugin).unwrap(), modified);
        assert!(!backup_path(&plugin).exists());
    }

    #[test]
    fn opencode_provenance_plugin_covers_mcp_and_direct_shell_calls() {
        assert!(is_whole_file_managed_provenance_plugin(
            OPENCODE_PROVENANCE_PLUGIN
        ));
        for expected in [
            "rtrt_agent_call",
            "rtrt_agent_route",
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
    fn opencode_provenance_plugin_refuses_unlisted_internal_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let edited = OPENCODE_PROVENANCE_PLUGIN.replacen(
            "const RTRT_AGENT_TOOLS",
            "Const RTRT_AGENT_TOOLS",
            1,
        );
        std::fs::write(&path, &edited).unwrap();

        let error = install_opencode_provenance_plugin_at(&path, true).unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
        assert!(!backup_path(&path).exists());
        assert!(!opencode_provenance_state_path(&path).unwrap().exists());
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
    fn opencode_provenance_plugin_uninstall_restores_internally_edited_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let edited = OPENCODE_PROVENANCE_PLUGIN.replacen(
            "const RTRT_AGENT_TOOLS",
            "Const RTRT_AGENT_TOOLS",
            1,
        );
        std::fs::write(backup_path(&path), &edited).unwrap();
        std::fs::write(&path, managed_opencode_provenance_plugin()).unwrap();

        remove_opencode_provenance_plugin_at(&path, true).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
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
    fn opencode_provenance_plugin_uninstall_removes_valid_orphan_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        write_opencode_provenance_state(&state_path, &path, &managed_opencode_provenance_plugin())
            .unwrap();

        remove_opencode_provenance_plugin_at(&path, true).unwrap();

        assert!(!state_path.exists());
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_dry_run_preserves_valid_orphan_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        write_opencode_provenance_state(&state_path, &path, &managed_opencode_provenance_plugin())
            .unwrap();

        remove_opencode_provenance_plugin_at(&path, false).unwrap();

        assert!(state_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn opencode_provenance_preflight_rejects_plugin_symlink_without_mutation() {
        use std::os::unix::fs::symlink;

        // Given
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let target = dir.path().join("plugin-target.js");
        let original = OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2.as_bytes();
        std::fs::write(&target, original).unwrap();
        symlink(&target, &path).unwrap();

        // When
        let error =
            remove_opencode_provenance_plugin_at_with_policy(&path, false, false).unwrap_err();

        // Then
        assert!(
            error
                .to_string()
                .contains("provenance plugin is not a real file")
        );
        assert_eq!(std::fs::read(&target).unwrap(), original);
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!backup_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn opencode_provenance_preflight_rejects_ownership_state_symlink_without_mutation() {
        use std::os::unix::fs::symlink;

        // Given
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        let state_target = dir.path().join("state-target.json");
        let original_state = b"{}";
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2).unwrap();
        std::fs::write(&state_target, original_state).unwrap();
        symlink(&state_target, &state_path).unwrap();

        // When
        let error =
            remove_opencode_provenance_plugin_at_with_policy(&path, false, false).unwrap_err();

        // Then
        assert!(
            error
                .to_string()
                .contains("provenance ownership state is not a real file")
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2.as_bytes()
        );
        assert_eq!(std::fs::read(&state_target).unwrap(), original_state);
        assert!(
            std::fs::symlink_metadata(&state_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!backup_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn opencode_provenance_preflight_rejects_applicable_backup_symlink_without_mutation() {
        use std::os::unix::fs::symlink;

        // Given
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let backup = backup_path(&path);
        let backup_target = dir.path().join("backup-target.js");
        let original_backup = b"export const Foreign = true\n";
        std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2).unwrap();
        std::fs::write(&backup_target, original_backup).unwrap();
        symlink(&backup_target, &backup).unwrap();

        // When
        let error =
            remove_opencode_provenance_plugin_at_with_policy(&path, false, false).unwrap_err();

        // Then
        assert!(
            error
                .to_string()
                .contains("provenance backup is not a real file")
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2.as_bytes()
        );
        assert_eq!(std::fs::read(&backup_target).unwrap(), original_backup);
        assert!(
            std::fs::symlink_metadata(&backup)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn opencode_provenance_preserve_policy_ignores_backup_for_unrecognized_plugin() {
        use std::os::unix::fs::symlink;

        // Given
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let backup = backup_path(&path);
        let backup_target = dir.path().join("backup-target.js");
        let user_plugin = b"export const UserPlugin = true\n";
        let original_backup = b"export const Foreign = true\n";
        std::fs::write(&path, user_plugin).unwrap();
        std::fs::write(&backup_target, original_backup).unwrap();
        symlink(&backup_target, &backup).unwrap();

        // When
        remove_opencode_provenance_plugin_at_with_policy(&path, true, true).unwrap();

        // Then
        assert_eq!(std::fs::read(&path).unwrap(), user_plugin);
        assert_eq!(std::fs::read(&backup_target).unwrap(), original_backup);
        assert!(
            std::fs::symlink_metadata(&backup)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_protects_invalid_or_mismatched_orphan_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtrt-provenance.js");
        let state_path = opencode_provenance_state_path(&path).unwrap();
        std::fs::write(&state_path, "[]").unwrap();
        assert!(remove_opencode_provenance_plugin_at(&path, true).is_err());
        assert!(state_path.exists());

        let state = serde_json::json!({
            "owner": OPENCODE_PROVENANCE_STATE_OWNER,
            "version": OPENCODE_PROVENANCE_STATE_VERSION,
            "path": dir.path().join("other.js").to_str().unwrap(),
            "content": OPENCODE_PROVENANCE_PLUGIN,
        });
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(remove_opencode_provenance_plugin_at(&path, true).is_err());
        assert!(state_path.exists());
    }

    #[test]
    fn opencode_provenance_plugin_uninstall_discards_exact_managed_backups() {
        for backup in [
            OPENCODE_PROVENANCE_PLUGIN,
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V1,
            OPENCODE_PROVENANCE_PLUGIN_LEGACY_V2,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rtrt-provenance.js");
            std::fs::write(&path, OPENCODE_PROVENANCE_PLUGIN).unwrap();
            std::fs::write(backup_path(&path), backup).unwrap();

            remove_opencode_provenance_plugin_at(&path, true).unwrap();

            assert!(!path.exists());
            assert!(!backup_path(&path).exists());
        }
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

        let resolved = resolve_opencode_config_path_in(&opencode_dir);

        assert_eq!(resolved, opencode_dir.join("opencode.json"));
    }

    #[test]
    fn resolve_opencode_config_path_falls_back_to_jsonc_when_only_it_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opencode_dir = dir.path().join(".config/opencode");
        std::fs::create_dir_all(&opencode_dir).unwrap();
        std::fs::write(opencode_dir.join("opencode.jsonc"), "{}").unwrap();

        let resolved = resolve_opencode_config_path_in(&opencode_dir);

        assert_eq!(resolved, opencode_dir.join("opencode.jsonc"));
    }

    #[test]
    fn resolve_opencode_config_path_defaults_to_json_when_neither_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opencode_dir = dir.path().join(".config/opencode");

        let resolved = resolve_opencode_config_path_in(&opencode_dir);

        assert_eq!(resolved, dir.path().join(".config/opencode/opencode.json"));
    }

    #[test]
    fn opencode_config_root_honors_nonempty_environment_precedence() {
        let opencode = Path::new("/custom/opencode");
        let xdg = Path::new("/xdg");
        let home = Path::new("/home/user");
        let profile = Path::new("C:/Users/user");

        assert_eq!(
            resolve_opencode_config_root_from(Some(opencode), Some(xdg), Some(home), Some(profile))
                .unwrap(),
            opencode
        );
        assert_eq!(
            resolve_opencode_config_root_from(
                Some(Path::new("")),
                Some(xdg),
                Some(home),
                Some(profile)
            )
            .unwrap(),
            xdg.join("opencode")
        );
        assert_eq!(
            resolve_opencode_config_root_from(None, Some(Path::new("")), Some(home), Some(profile))
                .unwrap(),
            home.join(".config/opencode")
        );
        assert_eq!(
            resolve_opencode_config_root_from(None, None, Some(Path::new("")), Some(profile))
                .unwrap(),
            profile.join(".config/opencode")
        );
        assert!(resolve_opencode_config_root_from(None, None, None, None).is_err());
    }

    #[test]
    fn opencode_managed_paths_share_the_resolved_config_root() {
        let root = Path::new("/custom/opencode");

        assert_eq!(
            resolve_opencode_config_path_in(root),
            root.join("opencode.json")
        );
        assert_eq!(
            resolve_opencode_tui_config_path_in(root),
            root.join("tui.json")
        );
        assert_eq!(opencode_rules_path_in(root), root.join("AGENTS.md"));
        assert_eq!(
            opencode_provenance_plugin_path_in(root),
            root.join("plugins/rtrt-provenance.js")
        );
        assert_eq!(opencode_tui_root_in(root), root.join("tui"));
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
            "[dry-run] would register rtrt-agent@0.1.1 and migrate legacy registrations file:///home/test/OpenCode%20Config/plugins/rtrt-provenance.js and ./plugins/rtrt-provenance.js"
        );
    }

    #[test]
    fn opencode_owned_package_registration_is_release_pinned() {
        assert_eq!(
            OPENCODE_NPM_PLUGIN_ID,
            concat!("rtrt-agent@", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(OPENCODE_NPM_PLUGIN_ID, "rtrt-agent@0.1.1");
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
                "--admin",
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
        assert_eq!(
            parsed["plugin"],
            serde_json::json!([OPENCODE_NPM_PLUGIN_ID])
        );
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
        assert_eq!(
            parsed["plugin"],
            serde_json::json!([OPENCODE_NPM_PLUGIN_ID])
        );
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
        assert!(first.contains(&format!("\"plugin\": [\"{OPENCODE_NPM_PLUGIN_ID}\"]")));

        apply_opencode_jsonc_at(&path, true, "/usr/local/bin/rtrt-mcp", &None)
            .expect("second apply should succeed");
        let second = std::fs::read_to_string(&path).expect("read after second apply");
        assert_eq!(
            first, second,
            "re-running apply on a JSONC file must not duplicate the rtrt-agent block"
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
                ["foreign-b", {"x": 1}],
                OPENCODE_NPM_PLUGIN_ID
            ])
        );
    }

    #[test]
    fn opencode_migration_canonicalizes_exact_package_forms_at_first_rtrt_agent_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let recognized = serde_json::json!([
            "rtrt-agent",
            "rtrt-agent@1.2.3",
            "rtrt-agent@latest",
            "rtrt-agent@^1",
            "rtrt-agent@~1",
            "rtrt-agent@<2",
            "rtrt-agent@>1",
            "rtrt-agent@=1",
            "rtrt-agent@*",
            ["rtrt-agent@next", {"trace": true}],
            {"package": "rtrt-agent@beta", "options": {"trace": true}},
            {"package": "rtrt-agent", "options": {"trace": true}}
        ]);
        let mut plugins = vec![serde_json::json!("foreign-before")];
        plugins.extend(recognized.as_array().unwrap().iter().cloned());
        plugins.push(serde_json::json!(["foreign-after", {"keep": true}]));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"plugin": plugins})).unwrap(),
        )
        .unwrap();

        apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["plugin"],
            serde_json::json!([
                "foreign-before",
                OPENCODE_NPM_PLUGIN_ID,
                ["foreign-after", {"keep": true}]
            ])
        );
    }

    #[test]
    fn opencode_migration_preserves_malformed_and_foreign_package_shapes_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let foreign = serde_json::json!([
            "",
            "rtrt-agent@",
            "rtrt",
            "rtrt@latest",
            ["rtrt@next", {"trace": true}],
            {"package": "rtrt@beta", "options": {"trace": true}},
            {"package": "rtrt", "options": {"trace": true}},
            "rtrt-extra@1",
            ["rtrt-agent@1"],
            ["rtrt-agent@1", "options"],
            ["rtrt-agent@1", {}, "extra"],
            {"package": "rtrt-agent", "options": false},
            {"package": "rtrt-agent", "extra": true},
            {"package": "rtrt-agent", "options": {}, "extra": true},
            "rtrt-opencode",
            "rtrt-opencode@latest",
            "rtrt-opencode@^1",
            ["rtrt-opencode@next", {"trace": true}],
            {"package": "rtrt-opencode@beta", "options": {"trace": true}},
            {"package": "rtrt-opencode", "options": {"trace": true}},
            {"package": 7},
            [OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID, {}],
            {"package": OPENCODE_PROVENANCE_PLUGIN_LEGACY_ID}
        ]);
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"plugin": foreign})).unwrap(),
        )
        .unwrap();

        apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut expected = foreign.as_array().unwrap().clone();
        expected.push(serde_json::json!(OPENCODE_NPM_PLUGIN_ID));
        assert_eq!(root["plugin"], serde_json::Value::Array(expected));
    }

    #[test]
    fn opencode_uninstall_removes_all_exact_rtrt_agent_package_forms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let foreign = serde_json::json!([
            "rtrt",
            "rtrt@latest",
            "rtrt@^1",
            ["rtrt@next", {"trace": true}],
            {"package": "rtrt@beta", "options": {}},
            {"package": "rtrt", "options": {"trace": true}},
            "rtrt-opencode",
            "rtrt-opencode@latest",
            "rtrt-opencode@^1",
            ["rtrt-opencode@next", {"trace": true}],
            {"package": "rtrt-opencode@beta", "options": {}},
            {"package": "rtrt-opencode", "options": {"trace": true}}
        ]);
        let mut plugins = vec![
            serde_json::json!("foreign-before"),
            serde_json::json!("rtrt-agent"),
            serde_json::json!("rtrt-agent@latest"),
            serde_json::json!(["rtrt-agent@next", {"trace": true}]),
            serde_json::json!({"package": "rtrt-agent@beta", "options": {}}),
        ];
        plugins.extend(foreign.as_array().unwrap().iter().cloned());
        plugins.push(serde_json::json!("foreign-after"));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"plugin": plugins})).unwrap(),
        )
        .unwrap();

        drop_opencode_jsonc_at(&path, true).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["plugin"],
            serde_json::json!([
                "foreign-before",
                "rtrt",
                "rtrt@latest",
                "rtrt@^1",
                ["rtrt@next", {"trace": true}],
                {"package": "rtrt@beta", "options": {}},
                {"package": "rtrt", "options": {"trace": true}},
                "rtrt-opencode",
                "rtrt-opencode@latest",
                "rtrt-opencode@^1",
                ["rtrt-opencode@next", {"trace": true}],
                {"package": "rtrt-opencode@beta", "options": {}},
                {"package": "rtrt-opencode", "options": {"trace": true}},
                "foreign-after"
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
                OPENCODE_NPM_PLUGIN_ID,
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
    fn opencode_migration_rejects_non_array_plugin_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.jsonc");
        let original = r#"{"plugin":"foreign","mcp":{"other":{}}}"#;
        std::fs::write(&path, original).unwrap();

        let error = apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap_err();

        assert!(error.to_string().contains("plugin is not an array"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
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
                    "rtrt-agent",
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
    fn opencode_migration_keeps_one_canonical_package_and_all_foreign_shapes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        let plugin_url = opencode_provenance_plugin_url(&path).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({
                "plugin": [
                    {"name": "foreign-object"},
                    "rtrt-opencode",
                    ["./tui/rtrt-statusline.tsx", {"bin": "/bin/rtrt"}],
                    plugin_url,
                    "foreign-string",
                    "rtrt-opencode"
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
                {"name": "foreign-object"},
                "rtrt-opencode",
                ["./tui/rtrt-statusline.tsx", {"bin": "/bin/rtrt"}],
                OPENCODE_NPM_PLUGIN_ID,
                "foreign-string",
                "rtrt-opencode"
            ])
        );
    }

    #[test]
    fn resolve_opencode_tui_config_path_prefers_json_then_jsonc() {
        let dir = tempfile::tempdir().unwrap();
        let opencode = dir.path().join(".config/opencode");
        std::fs::create_dir_all(&opencode).unwrap();

        assert_eq!(
            resolve_opencode_tui_config_path_in(&opencode),
            opencode.join("tui.json")
        );
        std::fs::write(opencode.join("tui.jsonc"), "{}").unwrap();
        assert_eq!(
            resolve_opencode_tui_config_path_in(&opencode),
            opencode.join("tui.jsonc")
        );
        std::fs::write(opencode.join("tui.json"), "{}").unwrap();
        assert_eq!(
            resolve_opencode_tui_config_path_in(&opencode),
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

    #[cfg(unix)]
    #[test]
    fn opencode_rules_refuse_direct_and_dangling_target_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        for dangling in [false, true] {
            let outside = dir.path().join(format!("outside-rules-{dangling}"));
            if !dangling {
                std::fs::write(&outside, "foreign rules\n").unwrap();
            }
            let path = dir.path().join(format!("AGENTS-{dangling}.md"));
            symlink(&outside, &path).unwrap();

            let error = install_opencode_agents_rules_at(&path, true).unwrap_err();

            assert!(error.to_string().contains("symlink"));
            if dangling {
                assert!(!outside.exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(&outside).unwrap(),
                    "foreign rules\n"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn opencode_rules_refuse_direct_and_dangling_backup_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        for dangling in [false, true] {
            let path = dir.path().join(format!("AGENTS-{dangling}.md"));
            std::fs::write(&path, "foreign rules\n").unwrap();
            let outside = dir.path().join(format!("outside-rules-backup-{dangling}"));
            if !dangling {
                std::fs::write(&outside, "outside sentinel\n").unwrap();
            }
            symlink(&outside, backup_path(&path)).unwrap();

            let error = install_opencode_agents_rules_at(&path, true).unwrap_err();

            assert!(error.to_string().contains("symlink"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "foreign rules\n");
            if dangling {
                assert!(!outside.exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(&outside).unwrap(),
                    "outside sentinel\n"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn opencode_tui_files_refuse_direct_and_dangling_target_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let current = managed_tui_statusline_source();
        for dangling in [false, true] {
            let outside = dir.path().join(format!("outside-tui-{dangling}"));
            if !dangling {
                std::fs::write(&outside, "foreign tui\n").unwrap();
            }
            let path = dir.path().join(format!("statusline-{dangling}.tsx"));
            symlink(&outside, &path).unwrap();

            let error = install_managed_tui_file_at(
                &path,
                &current,
                OPENCODE_TUI_STATUSLINE_BEGIN,
                OPENCODE_TUI_STATUSLINE_END,
                true,
            )
            .unwrap_err();

            assert!(error.to_string().contains("symlink"));
            if dangling {
                assert!(!outside.exists());
            } else {
                assert_eq!(std::fs::read_to_string(&outside).unwrap(), "foreign tui\n");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn opencode_tui_files_refuse_direct_and_dangling_backup_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let current = managed_tui_statusline_source();
        for dangling in [false, true] {
            let path = dir.path().join(format!("statusline-{dangling}.tsx"));
            let old = managed_tui_source(
                "old RTRT version",
                OPENCODE_TUI_STATUSLINE_BEGIN,
                OPENCODE_TUI_STATUSLINE_END,
            );
            std::fs::write(&path, &old).unwrap();
            let outside = dir.path().join(format!("outside-tui-backup-{dangling}"));
            if !dangling {
                std::fs::write(&outside, "outside sentinel\n").unwrap();
            }
            symlink(&outside, backup_path(&path)).unwrap();

            let error = install_managed_tui_file_at(
                &path,
                &current,
                OPENCODE_TUI_STATUSLINE_BEGIN,
                OPENCODE_TUI_STATUSLINE_END,
                true,
            )
            .unwrap_err();

            assert!(error.to_string().contains("symlink"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
            if dangling {
                assert!(!outside.exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(&outside).unwrap(),
                    "outside sentinel\n"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn opencode_config_refuses_direct_and_dangling_target_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        for dangling in [false, true] {
            let outside = dir.path().join(format!("outside-config-{dangling}"));
            if !dangling {
                std::fs::write(&outside, r#"{"foreign":true}"#).unwrap();
            }
            let path = dir.path().join(format!("opencode-{dangling}.json"));
            symlink(&outside, &path).unwrap();

            let error = apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap_err();

            assert!(error.to_string().contains("symlink"));
            if dangling {
                assert!(!outside.exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(&outside).unwrap(),
                    r#"{"foreign":true}"#
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn opencode_config_refuses_direct_and_dangling_backup_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        for dangling in [false, true] {
            let path = dir.path().join(format!("opencode-{dangling}.json"));
            let original = r#"{"foreign":true}"#;
            std::fs::write(&path, original).unwrap();
            let outside = dir.path().join(format!("outside-config-backup-{dangling}"));
            if !dangling {
                std::fs::write(&outside, "outside sentinel\n").unwrap();
            }
            symlink(&outside, backup_path(&path)).unwrap();

            let error = apply_opencode_jsonc_at(&path, true, "/bin/rtrt-mcp", &None).unwrap_err();

            assert!(error.to_string().contains("symlink"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            if dangling {
                assert!(!outside.exists());
            } else {
                assert_eq!(
                    std::fs::read_to_string(&outside).unwrap(),
                    "outside sentinel\n"
                );
            }
        }
    }

    #[test]
    fn private_atomic_write_replaces_existing_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("managed.json");
        std::fs::write(&path, b"old").unwrap();

        write_private_file_atomic_same_dir(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }
}
