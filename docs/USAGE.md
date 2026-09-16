# Usage

**English** | [한국어](USAGE.ko.md)

This page documents the `rtrt` CLI, the `rtrt-mcp` server, and the `rtrt-dashboard` web UI as of v0.1.3.

## CLI

```text
rtrt --help
```

### `rtrt compress`

Compress text read from stdin and write to stdout.

```bash
# rule-based (default)
echo "Sure, I'd be happy to help. The bug is really in the parser." \
  | rtrt compress -l ultra

# LLM-backed (any provider; Ollama shown)
echo "I think the bug is, perhaps, in the parser..." | rtrt compress --llm \
  --provider openai-compat --base-url http://127.0.0.1:11434/v1 --model llama3.2
```

Flags:

- `-l, --level <lite|full|ultra|extreme>` — compression intensity. Default `full`.
- `--ml` — use the LLMLingua-style token-importance compressor instead of the rule pass; mutually exclusive with `--llm`. Pair with `--ratio <0.05..=1.0>` (default `0.5`).
- `--format <plain|markdown|xml|json>` — chroma-style framing for the rule output. Default `plain`.

Rules per level (cumulative):

- `lite` — fillers (`just`, `really`, `basically`, `actually`, `simply`, `literally`, `honestly`, `frankly`, `truly`, `essentially`, `kind of`, `sort of`) + multi-space and multi-newline collapse.
- `full` — `lite` + pleasantries (`sure`, `certainly`, `of course`, `happy to`, `let me`, `I'll`, `I can`, `I would`, `I'd be happy to`) + hedging (`I think / believe / suspect / guess`, `in my opinion`, `perhaps / maybe / probably / possibly`, `it seems / appears`, `if I recall correctly`) + discourse markers (`moreover`, `furthermore`, `however`, `nevertheless`, `as you can see`, `needless to say`, `it's worth noting that`, `of course`, `obviously`, `clearly`) + meta-phrases (`it is important to note that`, `it should be noted that`, `as we mentioned earlier`).
- `ultra` — `full` + articles (`a`, `an`, `the`) + phrase shortening (`due to the fact that` → `because`, `in order to` → `to`, `at this point in time` → `now`, `for the purpose of` → `for`, `in the event that` → `if`, `with the exception of` → `except`, `a number of` → `several`, `the majority of` → `most`, `in spite of` → `despite`, `on the basis of` → `based on`, `for instance` → `e.g.`).
- `extreme` — `ultra` + verbose qualifiers (`very`, `extremely`, `quite`, `rather`, `fairly`, `somewhat`, `highly`).

Code blocks (` ``` ` and ` ` `), URLs, and `"quoted strings"` are stashed before the rule pass and restored afterwards, so technical content is never rewritten. Secret-shaped substrings (AWS / GitHub / OpenAI / Anthropic / Slack / Bearer / private-key / `api_key=…`) are replaced with `<REDACTED:<kind>>` **before** the rule pass.

### `rtrt signatures`

Strip function bodies from source via tree-sitter, keep top-level signatures
only. Best for code-heavy LLM context windows.

```bash
rtrt signatures --lang rust < crates/rtrt-providers/src/anthropic.rs
# 8972 bytes → 1948 bytes  (78% saved on a real file)
```

Currently supports `--lang rust`. Other languages can be added by enabling the
matching `tree-sitter-<lang>` grammar; see `crates/rtrt-compress/src/treesitter.rs`.

### `rtrt proxy`

Filter a command's stdout for a known command name.

```bash
git status | rtrt proxy "git status"
cargo build 2>&1 | rtrt proxy "cargo build"
```

### `rtrt proxy-run`

Run a command and filter its captured output before it reaches the agent. `proxy-run` preserves the wrapped command's exit code.

```bash
rtrt proxy-run git status
rtrt proxy-run cargo test -p rtrt-memory
rtrt proxy-run --errors-only npm test
rtrt proxy-run --ultra-compact docker ps
rtrt proxy-run --raw cargo build
```

Flags:

- `--raw` — print captured output unchanged while still recording the run.
- `--errors-only` — keep likely error and warning lines when no command-specific filter matches.
- `--ultra-compact` — strip ANSI escapes and collapse repeated lines when no command-specific filter matches.

Built-in filter rules cover 34 command patterns across `git status/diff/show/branch/stash/log`, `cargo check/clippy/build/test/nextest`, `ls`, `grep`, `rg`, `find`, `cat`, `curl`, `wget`, `gh`, `docker`, `kubectl`, `pytest`, `go test`, `npm` / `npx` / `pnpm`, `pip`, `tsc`, `eslint`, and `prettier`. The filters are split into per-domain modules.

`rtrt proxy` remains available for explicit pipe workflows. When the command does not match a built-in, output passes through unchanged.

### `rtrt hook proxy-rewrite`

Claude Code can run the Command Optimizer transparently through a `PreToolUse` Bash hook:

```bash
rtrt setup --agent claude --apply
```

The installed matcher targets Bash tool calls and rewrites shrinkable commands to `rtrt proxy-run ...`. It skips commands that already include pipes, `&&`, redirects, or an existing `rtrt proxy-run` wrapper. Cursor, Codex, Windsurf, opencode, and other MCP-aware agents receive the Command Optimizer tools through `rtrt-mcp` instead of the Claude-specific hook.

The hook implementation can also be invoked directly by hook runners:

```bash
rtrt hook proxy-rewrite
```

### Multi-agent coordination boundary

RTRT does not provide a team command, scheduler, roster, worker protocol, or `team_dispatch` MCP tool. Multi-agent coordination belongs to the external agent runtime. RTRT remains responsible for compression, memory, provider routing and failover, security scanning, setup integration, and provenance.

### OpenCode npm plugin and setup migration

Install `rtrt-agent@0.1.3` with `npm install rtrt-agent@0.1.3` and register it directly with OpenCode's singular root `plugin` key:

```json
{ "plugin": ["rtrt-agent@0.1.3"] }
```

The npm package exports RTRT's provenance and permission hooks and starts the version-matched dashboard backend as a detached, loopback-only process. Plugin initialization does not wait for it and does not open a browser. Run `rtrt-dashboard-open`, or explicitly ask the agent to use `rtrt_dashboard_open`, when the browser is needed. The TUI statusline is not shipped in npm and remains setup-managed.

For a complete installation, prefer:

```bash
rtrt setup --agent opencode --apply
```

Setup performs no npm installation itself. It first writes the exact `rtrt-agent@0.1.3` registration and every replacement managed asset. Only after all of those writes succeed does it perform final cleanup of recognized legacy RTRT plugin entries; a failure before that point preserves the legacy runtime. OpenCode installs the configured npm package and its matching platform dashboard package when it starts. Dashboard startup is fail-soft and preserves existing `~/.rtrt` data. Foreign plugin strings, tuples, objects, and unrecognized legacy entries retain their order and content. Uninstall removes only RTRT-owned entries. The resolved config root is the first nonempty value of `OPENCODE_CONFIG_DIR`, then `$XDG_CONFIG_HOME/opencode`, then the HOME/USERPROFILE fallback root, `~/.config/opencode` on HOME-based systems. Coexistence is CI-gated against OMO 4.19.4 and was verified on OpenCode 1.18.29; neither is a promise for future versions. The unified release workflow is responsible for publishing the version-matched Rust artifacts and npm packages; this documentation does not assert that publication has already completed.

### OpenCode persistent statusline

```bash
rtrt setup --agent opencode --apply
# Restart OpenCode after setup.
```

Setup installs two managed TUI files under the resolved OpenCode config root:

- `tui/rtrt-statusline.tsx`
- `tui/rtrt-statusline-core.mjs`

It also adds one tuple to the active OpenCode TUI config's `plugin` array:

```json
["./tui/rtrt-statusline.tsx", {"bin": "/absolute/path/to/rtrt"}]
```

The config resolver prefers an existing `tui.json` under that root, then an existing `tui.jsonc`, and creates `tui.json` there when neither exists. Setup parses and merges the document instead of replacing the plugin array: foreign plugins, unrelated keys, and existing non-`bin` options on the RTRT tuple survive. Repeated setup is idempotent. An unrecognized pre-existing file at either managed TUI path is not overwritten.

The plugin registers one persistent `app_bottom` surface at the bottom of the application. It deliberately does not register `session_prompt_right`: keeping the statusline outside OpenCode's prompt render path prevents streaming updates from delaying keyboard input or interrupt handling.

The line refreshes immediately at startup, after scoped project and session lifecycle/status events (750 ms burst debounce), and every 15 seconds. Session events refresh only the active application line. Session economics are read as non-reactive snapshots during those scoped events; high-volume file and message-part updates neither rerender the statusline nor spawn statusline work. Refreshes never overlap. TUI plugins load at OpenCode process startup, so restart OpenCode after installation or upgrade; already-running processes do not acquire the statusline.

#### OpenCode JSON contract

The TUI invokes `rtrt statusline --opencode` without a shell. You can inspect the same one-line compact JSON contract directly:

```bash
rtrt statusline --opencode --cwd "$PWD" --session session-id --width 120
rtrt statusline --opencode --cwd "$PWD" --width 80 --budget-ms 120 --no-git
rtrt statusline --opencode --cwd "$PWD" --width 120 --refresh
```

The command never waits for or reads stdin. Its version-1 object contains `v`, `ts`, `took_ms`, `stale`, `degraded`, `project`, `cwd`, `data`, and prioritized `segments`; each segment carries `id`, `text`, `tone`, and `pri`. `--refresh` bypasses preferred fresh snapshots, while `--no-git` disables Git collection even at wide widths.

| Width | Eligible display segments |
|-------|---------------------------|
| `< 60` | Overall savings (`Σ`) when available, plus Output Optimizer style |
| `60-99` | Project, style, savings, and provider headroom |
| `>= 100` | All above, plus Git, optional model, session, and memory aggregate |

The TUI additionally drops lower-priority segments until the rendered text fits. OpenCode SDK 1.18.13 exposes the current selection as `Session.model`. The plugin resolves its provider and model from session state, accepts only a safe `provider/model`, and forwards it to the snapshot as argv-only `--model`; it does not infer the selection from historical messages. Manual callers may also pass `--model <provider/model>`.

#### Session metrics

Session metrics are derived from OpenCode SDK state, independently of RTRT's CLI usage ledger:

| Display | Semantics |
|---------|-----------|
| `MODEL` | Current `Session.model`, resolved through the SDK provider catalog. A safe `provider/model` is forwarded to the RTRT snapshot. |
| `COST` | OpenCode-reported/computed `Session.cost` estimate. A positive value renders as `~$<amount>`; zero renders as `$0?` because included, free, and unpriced usage are indistinguishable; an absent value renders as `N/A`. |
| `CTX` | Percentage from the latest completed, non-error assistant turn with output: `(input + output + reasoning + cache read + cache write) / context limit` for that turn's exact provider/model. It is not clamped at 100%, so overflow remains visible; an unavailable exact-model limit renders as `N/A`. |
| `STATE` | Current session status: `BUSY`, `RETRY`, or `IDLE`. OpenCode omits idle sessions from its active status map, so a known session with no active status renders as `IDLE`. |
| `5H` | Official Claude Code `rate_limits.five_hour` usage and reset countdown when bridge cache is fresh and its reset has not expired; otherwise `N/A`. |
| `WEEK` | Official Claude Code `rate_limits.seven_day` usage and reset countdown under same freshness/reset rules; otherwise `N/A/not exposed`. |

`COST` and `CTX` keep session semantics above. Rate-limit bridge does not derive either value or replace them with account data.

#### Official Claude Code rate-limit bridge

Claude Code supplies an official `rate_limits` object to its configured statusline command. `rtrt setup --agent claude --apply` installs that source/writer; OpenCode setup installs only reader/display, so running OpenCode alone does not refresh cache. When `rtrt statusline --rich` receives payload, it writes only cache version, capture time, and each available window's numeric `used_percentage` and `resets_at`. It does not cache credentials, OAuth tokens, session or prompt identifiers, transcript paths/content, context/token counts, or any other Claude statusline fields. OpenCode's `rtrt statusline --opencode` collector only reads this local cache; it performs no credential lookup and makes no network request.

Default cache is private `~/.rtrt/statusline/claude-rate-limits.json`. On Unix, RTRT writes it as mode `0600` beneath a mode `0700` directory and rejects unsafe links or permissions when reading it. A window is eligible for OpenCode only when Claude statusline refreshed within default 15-minute maximum age and `resets_at` is still in future. Each window is checked independently. Stale, expired, absent, malformed, or unsafe cache data is treated as `N/A`, never as zero or estimated quota.

| Environment variable | Default | Purpose |
|----------------------|---------|---------|
| `RTRT_CLAUDE_RATE_LIMIT_CACHE` | `~/.rtrt/statusline/claude-rate-limits.json` | Override private bridge-cache path for both Claude writer and OpenCode reader. |
| `RTRT_CLAUDE_RATE_LIMIT_MAX_AGE_SEC` | `900` | Override accepted cache age in seconds (`1` through `86400`). Invalid values use `900`. |

This bridges same official `rate_limits` data Claude Code itself provides; it is not a separate quota API. RTRT intentionally does not poll an undocumented OAuth endpoint: doing so would require acquiring, storing, or sending Claude credentials and would depend on unsupported Terms-of-Service behavior and unstable response schema.

`WEEK` is official provider window from `rate_limits.seven_day`. In contrast, RTRT's rolling 7d provider-usage ledger records only invocations observed locally. That ledger is useful activity history, but is not provider quota, does not populate `5H` or `WEEK`, and is never used as their fallback.

#### Local collection and failure states

Collection is local-only and best-effort. Default CLI wall budget is 120 ms; the TUI also kills an unresponsive CLI child after 1.5 seconds. Collectors read only effective local config, bounded local savings/usage files and caches, read-only SQLite with zero busy timeout and deadline interruption, and a local Git status capped at 50 ms. Git disables optional locks, filesystem monitor, untracked-file enumeration, ahead/behind work, and submodule work. Statusline collection makes no network request and scans no OpenCode or Claude transcript.

`degraded` and `stale` have different meanings:

- `degraded` lists width-eligible collectors that were unavailable or could not finish inside the budget, plus conditions such as a non-canonical `cwd` or expired `budget`. Other valid segments still render, so a partial result is not a command failure.
- `stale: true` means the wall budget expired or a stale Git cache had to be used. The TUI dims the entire line and appends `stale`.
- If a later child invocation times out, exits unsuccessfully, or returns invalid JSON, the TUI keeps the last good payload, dims it, and appends `stale`.
- If no good payload has ever been received, the fallback is the dim `rtrt · n/a` line.

#### Uninstall

```bash
rtrt uninstall --agent opencode --apply
# Restart OpenCode after uninstall.
```

Uninstall ordering is deliberate: stop/remove RTRT services and integrations first, then remove managed binaries/files. OpenCode uninstall removes the RTRT tuple from `tui.json` / `tui.jsonc` under the resolved config root and removes only recognized RTRT-managed blocks from the two TUI files there. Foreign plugins, unrelated config keys, and non-RTRT file content remain. Modified or unrecognized managed-file content is preserved rather than deleted. The command also removes the other RTRT-managed OpenCode rules, provenance plugin/bridge, and `mcp.rtrt` entry. Restart is required for an existing OpenCode process to unload the TUI plugin. Typed managed paths and MCP entries reject symlinks and unsafe ownership/type changes.

### OpenCode-to-Claude provenance

```bash
rtrt setup --agent opencode --apply
```

Setup installs a global OpenCode plugin. It does not read, write, or require global `~/.claude.json`, and it does not install a global Claude provenance hook. Each exact direct `claude -p` invocation disables global, user, and project Claude setting sources, then injects one ephemeral strict settings object containing the exact `SessionStart` provenance hook. The invocation also injects one strict permission-only RTRT MCP config; existing foreign/shared Claude MCP configuration is irrelevant and preserved. The plugin assigns a stable invocation UUID per tool call and propagates parent project/session/call, active agent, cwd, and worktree through RTRT MCP arguments and direct shell environments. RTRT-generated calls receive an explicit child session ID; the injected hook stores the first parent owner for that child without allowing a later resume to overwrite it. Transcript capture and boot-time reattribution prefer this durable join over path inference, while MCP auto-capture can fall back to the propagated parent project. Restart OpenCode after installation.

### OpenCode-to-Claude permission prompt bridge

Direct Claude CLI lanes launch outside RTRT's Linux bwrap shell confinement and use the canonical flag:

```text
--permission-prompt-tool mcp__rtrt__permission_prompt
```

Each exact argv launch disables global/user/project setting sources and injects one
ephemeral strict settings object containing Claude Code's official sandbox settings
`enabled=true`, `failIfUnavailable=true`, and `allowUnsandboxedCommands=false`,
with a strict network allowlist, project-only home-read exception,
credential/environment scrubbing, the exact `SessionStart` provenance hook, and
one strict permission-only RTRT MCP config. Missing
Claude Linux dependencies fail closed; RTRT never installs them. `socat` is an
optional Claude Code host prerequisite only, never bundled or installed by
RTRT, and setup does not imply user approval.

#### Strict Linux OpenCode shell confinement

`rtrt setup --agent opencode --sandbox --apply` is setup-owned confinement, not
a VM. It uses only a fixed, validated operator-installed `/usr/bin/bwrap` or
`/bin/bwrap`, with isolated namespaces/network, disabled nested user
namespaces, scrubbed environment, private `/tmp`, read-only system/tool caches,
and writable canonical project/Git metadata. The registry and project path are
validated against the invoking user and actual Git-worktree boundary. An
unsupported or unusable host fails closed. This confines the OpenCode shell;
direct Claude launches do not run inside RTRT bwrap.

This is an RTRT-only bridge. OpenCode setup does not touch Claude's global config or existing foreign/shared MCP entries. The broker is part of the existing provenance plugin—not a standalone daemon, script, service, or third-party plugin—and adds no dependency. It binds `127.0.0.1` on an ephemeral port, using a random token and nonce per invocation together with parent session/call identity.

`rtrt-mcp` forwards only bounded Claude tool-request fields. It never auto-captures or persists the raw prompt, credentials, token, nonce, or raw tool input. OpenCode v2 native permissions first evaluate the existing project/global policy, then show native once/always/reject UI when needed. Persistence for **always** belongs solely to OpenCode; RTRT does not create a second persistence policy. Approval has no wall-clock timeout: it waits like native OpenCode until a decision or lifecycle cancellation. Only connection establishment is briefly bounded. Malformed data, authentication or connection failure, tool/session cancellation, disconnect, and disposal default to deny.

Native Task inheritance is unchanged. OpenCode setup requires an executable outside the writable project root; use an installed `~/.cargo/bin/rtrt`, never a project `target/` binary. Verified Claude Code permission-prompt-tool support covers versions 2.1.219 through 2.1.221; the installed OpenCode SDK contract is 1.18.11. This does not claim a broader minimum compatibility range. After upgrading either tool, restart OpenCode and rerun `rtrt setup --agent opencode --apply`.

### Project-local temporary files

### Project-private OpenCode launcher

Run OpenCode from an external terminal through RTRT (arguments are accepted only after `--`):

```bash
rtrt opencode --project /path/to/checkout -- --model provider/model
# or, from the checkout:
rtrt opencode --
```

The launcher derives an immutable project identity from `--project` or cwd. Linked worktrees share identity/data, while OpenCode starts at the selected writable checkout boundary. Each distinct linked-worktree boundary is authorized separately; same-basename repositories remain distinct.

Eligible Linux/WSL installs automatically run `rtrt setup --agent opencode --sandbox --machine-only --apply`, which validates installed RTRT and fixed root-owned usable bubblewrap, creates an empty machine registry, and authorizes no cwd. `--no-setup` / `RTRT_NO_SETUP=1` opts out. Manual machine bootstrap is cwd-independent. Every later `rtrt opencode --` revalidates exact managed state and atomically authorizes only the explicitly launched canonical checkout under the shared registry lock. Tampering fails closed; launch never repairs global config or edits the repository.

It sets project-private `XDG_DATA_HOME`, `XDG_STATE_HOME`, and `OPENCODE_DB` beneath `~/.rtrt/projects/<slug>/opencode/`; global XDG config remains unchanged, so installed config, agents, and plugins remain available. Private directories use mode `0700`, launcher-created files use `0600`. A safe regular global `opencode/auth.json` is copied once only when the private destination is absent. The launcher rejects nested OpenCode/model-shell sessions, unsafe/symlink executables, and directory arguments selecting another project. It invokes the validated OpenCode executable directly without a shell.

Before first launch, migrate the machine-wide global SQLite session graph from any cwd:

```bash
rtrt opencode sessions status   # read-only probe
rtrt opencode sessions dry-run  # exact plan, no writes
rtrt opencode sessions apply    # locked, atomic, idempotent migration
```

Migration opens the original global database read-only with WAL visibility; it creates no full database/WAL snapshot and retains the source as backup. `session.directory` takes priority, then safe project metadata; canonical RTRT identity keeps linked worktrees together and same-basename repositories separate. Session IDs, parent/child graphs, messages, parts, todos, workspace/share/projection/event rows, explicit indexes, and other supported resume data are copied opaquely without inspecting prompt content. Duplicate rows in primary-key-less tables retain their exact multiplicity. Account, credential, control-account, and persistent permission/approval rows are excluded. Deleted or unattributable sessions are retained in private `legacy-global`; prompt-history JSONL remains separately preserved and is not claimed as project-attributable. Explicit `apply` is strict: conflicts roll back, and unsupported triggers/views fail closed rather than being omitted. Pre-launch incremental catch-up preserves conflicting private rows and copies safe missing rows, but every catch-up error or held lock produces only a content-free warning and never blocks an otherwise valid private launch. A content-free DB/WAL generation stamp skips unchanged source generations without missing later WAL growth.

Direct `opencode` remains globally stateful. Setup's `history_previous=none` and `history_next=none` disable TUI history navigation only; they do not stop global history writes. Inspect or explicitly quarantine only known prompt-history files:

```bash
rtrt opencode history-status
rtrt opencode history-quarantine          # dry-run
rtrt opencode history-quarantine --apply  # rename; never delete
```

Quarantine uses exact known paths and a `.rtrt-quarantine` sibling; it does not scan the home directory or destructively migrate OpenCode data.

Temporary-directory resolution is deterministic: `RTRT_TMP_DIR` overrides all defaults; otherwise a discovered project uses `<main-linked-repository-root>/.rtrt/tmp` (linked worktrees resolve to the main repository), and a run with no project uses a private per-user directory below OS temp (`<OS temp>/rtrt-<uid>` on Unix, with a platform-equivalent private per-user directory elsewhere), never the shared `<OS temp>/rtrt`. RTRT rejects symlinks and non-directory candidates; on Unix it also verifies current-user ownership and mode `0700`. If RTRT discovers a project but cannot create its local temporary directory, it returns an error instead of silently escaping to OS temp.

For each OpenCode session, the plugin assigns `<main-linked-repository-root>/.rtrt/tmp/opencode/<session>` to `TMPDIR`, `TEMP`, and `TMP`. This scopes child-process temporary files, but cannot relocate OpenCode native Task's internal worktree root because the current plugin SDK exposes no control for it. Already-running sessions under `/tmp/opencode` are unaffected; restart OpenCode to load the plugin change for new session environments.

### `rtrt gain`

Show Command Optimizer savings from `~/.rtrt/proxy-stats.sqlite`. Token counts are labelled estimates using `chars / 4`.

```bash
rtrt gain
rtrt gain --project rtrt
rtrt gain --history
rtrt gain --daily --graph
rtrt gain --weekly
rtrt gain --monthly
rtrt gain --format json
rtrt gain --reset --yes
```

The report includes totals, top commands, per-project totals, optional recent history, and daily / weekly / monthly bucketed views.

### `rtrt discover`

Scan Claude Code transcripts for commands that can be shrunk by the Command Optimizer and estimate the possible savings.

```bash
rtrt discover
rtrt discover --project rtrt
rtrt discover --all --since 2026-06-01
rtrt discover --format json
```

### `rtrt route`

Pick — and optionally invoke — the cheapest useful route for a prompt. Ranking is cost-tier first (local-free → subscription-flat → API-metered), headroom-weighted inside each tier: candidates under ~15% remaining `[limits]` headroom are penalized, exhausted targets are demoted to last resort.

```bash
rtrt route --dry-run "summarise this diff"          # decision only, no invocation
rtrt route --explain "summarise this diff"          # decision + ranked alternatives + headroom
rtrt route --prefer local "quick sanity check"      # --prefer cheapest|quality|local
rtrt route --failover "must succeed"                # walk ranked candidates on retryable errors
rtrt route --target ollama --model llama3.2 "hi"    # explicit target override
```

### `rtrt call`

Invoke a detected local agent or provider through the cross-tool bridge.

```bash
rtrt call claude "explain this error"               # target from `rtrt detect`
rtrt call ollama --model llama3.2 "ping"
rtrt call codex --mode cli --timeout 60 "review"    # --mode auto|cli|api
rtrt call claude --failover "must succeed"          # fall over to the next ranked target
rtrt call claude --format json "ping"
```

`--failover` retries the next ranked target on retryable failures (rate-limit / quota / 429 / 5xx / timeout).

### `rtrt usage`

Show per-target windowed provider usage (5h / 24h / 7d) and the headroom remaining against the `[limits]` daily caps (see [Configuration file](#configuration-file)). Rows with estimated token counts (CLI shell-outs, ~chars/4) are marked `~`.

```bash
rtrt usage
rtrt usage --format json
```

The ledger lives at `~/.rtrt/provider-usage.tsv` (override with `RTRT_PROVIDER_USAGE_PATH`), capped at the most-recent 5000 rows.

### `rtrt security`

Profile-driven security + license scan (secrets / licenses / deps / patterns / AI-artifact engines). See [FEATURES.md](FEATURES.md#security--license-scanning) for the engine and profile details.

```bash
rtrt security scan --profile ai-default --path . --json
rtrt security profile list
rtrt security profile show owasp-top-10
rtrt security gate --profile ai-default    # CI gate: non-zero exit at/above threshold
rtrt security init                         # copy built-in profiles to ~/.rtrt/security/profiles/
```

### `rtrt migrate` / `rtrt project`

Migrate an existing repository to the rtrt project standard and keep it consistent. Both `migrate` and `project refresh` are dry-run by default — pass `--apply` to write.

```bash
rtrt migrate                        # plan only (dry-run)
rtrt migrate --apply                # apply the migration
rtrt project refresh --apply        # one-command alias: render contract → canonical settings → audit
rtrt project status                 # contract, agents, hooks, statusline, memory reachability
rtrt project health                 # status + deeper lifecycle consistency checks
rtrt project repair --dry-run       # append missing managed sections / install missing agents
```

Both `migrate` and `project refresh` strip project-level rtrt-owned key shadows (e.g. a project `.claude/settings.json` re-declaring `statusLine`) with a `.bak` backup so the project follows the global base kernel.

### `rtrt templates`

List available templates (built-in + custom).

```text
design              [BuiltIn]  Document chain that generates a design kit
dev                 [BuiltIn]  Document chain that generates a development starter set
plan                [BuiltIn]  Document chain that generates a planning set
standardization     [BuiltIn]  Project contract with CLAUDE.md and agent definitions
```

Custom templates live in `~/.rtrt/templates/<name>/manifest.toml` and appear under `[Custom]`.

### `rtrt new`

Scaffold a project from a template.

```bash
rtrt new dev ./hello \
  --var project_name=hello \
  --var author="Kim DaeHyun"
```

Flags:

- `--var key=value` — set a template variable (repeatable).
- `--overwrite` — replace existing files at the target path.
- `--no-hooks` — skip post-init shell hooks (e.g. `git init`, `npm install`).

If `--var project_name` is omitted, the target directory's name is used.

### `rtrt info`

Print the version and the workspace crate list.

### `rtrt memory`

SQLite-backed memory store (BM25 + optional vector + optional graph).

```bash
echo "claude flagged auth flow as risky" \
  | rtrt memory save --project rtrt --kind note
rtrt memory recall --project rtrt --query auth --limit 10 \
  --filter "source=claude,topic~^auth"
```

The `--filter` flag takes the qdrant-style payload DSL (`key=val`, `key!=val`, `key~regex`, comma-AND).

### `rtrt diagnose`

Run a command, apply `errors_only`, then hand the failure to an LLM for a one-shot root-cause + fix suggestion.

```bash
rtrt diagnose --provider anthropic --model claude-haiku-4-5 \
  -- cargo test -p rtrt-memory
```

### `rtrt mcp`

Launch the bundled MCP server without remembering the binary name.

```bash
RTRT_MCP_HTTP_TOKEN=$(openssl rand -hex 16) \
  rtrt mcp --transport http --bind 127.0.0.1:7312 \
  --allowed-origins https://app.example.com
```

### `rtrt gateway`

Run an OpenAI-compatible HTTP endpoint so **any** tool that speaks the OpenAI
wire format becomes an rtrt client. See [Gateway](#gateway-rtrt-gateway-serve).

```bash
rtrt gateway serve                       # http://127.0.0.1:7412/v1 (loopback)
rtrt gateway serve --port 8080 --host 127.0.0.1
RTRT_GATEWAY_TOKEN=$(openssl rand -hex 16) rtrt gateway serve
```

### `rtrt benchmark`

Wrap `cargo bench` so the published 60%+ savings claim is one command away.

```bash
rtrt benchmark                    # cargo bench -p rtrt-compress --bench compress_bench
rtrt benchmark --extra '--quick'
```

## Gateway (`rtrt gateway serve`)

Point any OpenAI-compatible client at `http://127.0.0.1:7412/v1` and rtrt
auto-routes each request across your detected providers — one env var turns
Cursor, the OpenAI SDK, `llm`, Continue, or any curl script into an rtrt
client. It binds loopback by default.

```bash
rtrt gateway serve --port 7412 --host 127.0.0.1
export OPENAI_BASE_URL=http://127.0.0.1:7412/v1
export OPENAI_API_KEY=unused      # or the RTRT_GATEWAY_TOKEN you set
```

Endpoints:

- `POST /v1/chat/completions` — OpenAI Chat Completions (request + response; SSE `chat.completion.chunk` stream when `stream:true`, ending with `data: [DONE]`).
- `GET /v1/models` — the routable pseudo-models plus every detected target/model.
- `GET /healthz` — liveness probe (always open, even with a token set).

The `model` field selects the routing strategy:

| `model` | Behaviour |
|---------|-----------|
| `auto` / `""` / `rtrt/auto` | Full route: infer a capability from the request, then headroom-aware `select_route` + automatic failover down the ranked targets. |
| `rtrt/cheapest` | Same ranked list, cheapest cost tier first. |
| `rtrt/best` | Same ranked list, highest-capability tier first. |
| `anthropic/claude-…`, `openai/gpt-…`, `ollama/…`, or a bare model id | Dispatched through the existing provider gateway by model-id prefix. |

Capability inference for the `auto` family is deliberately simple: a fenced
code block (```` ``` ````) routes as **code**, an otherwise long request (over
~2000 chars) routes as **reasoning**, everything else is general **chat**.

Every dispatch records to the same usage ledger the router balances on (via the
machinery it reuses), so there is no separate accounting and no double-counting.

Security:

- Binds `127.0.0.1` by default. A non-loopback bind without a token logs a warning.
- `--token <T>` / `RTRT_GATEWAY_TOKEN` requires `Authorization: Bearer <T>` on `/v1/*` (401 + `WWW-Authenticate` on miss; constant-time comparison). `/healthz` stays open.

Limitations (honest): this is a **text-only** bridge. Requests are flattened to
a single prompt for the routed path, so tool-calling / function-calling / vision
content is not passed through yet. Streaming is buffered — the routed answer is
computed in full and emitted as SSE chunks (CLI-mode targets only produce full
text), so `stream:true` is wire-compatible but not token-by-token.

```bash
# Auto-routed, non-streaming
curl http://127.0.0.1:7412/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hello"}]}'

# Explicit local model, streaming
curl -N http://127.0.0.1:7412/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"ollama/gemma3:4b","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

## MCP server (`rtrt-mcp`)

```bash
# stdio (default; what Claude Code / Codex / Cursor / Windsurf / opencode use)
rtrt-mcp --admin --memory ~/.rtrt/memory.sqlite

# Streamable HTTP (MCP 2025-06-18) behind axum
RTRT_MCP_HTTP_TOKEN=$(openssl rand -hex 16) \
  rtrt-mcp --transport http --bind 127.0.0.1:7312 --path /mcp
```

Implemented via [`rmcp`](https://crates.io/crates/rmcp), the official Rust MCP SDK. Tools currently shipped:

| Tool | Wraps | Notes |
|------|-------|-------|
| `compress` | `Compressor::compress` | `level = lite \| full \| ultra` (default `full`) |
| `compress_ml` | `MlCompressor::compress` | LLMLingua-style token-importance pruning; `ratio` ∈ (0.05, 1.0] |
| `proxy` | `rtrt_proxy::{filter_for, errors_only, ultra_compact}` | mode = `command \| errors_only \| ultra_compact` |
| `memory_save` | `MemoryStore::save` | FTS5 + BM25 index |
| `memory_recall` | `MemoryStore::recall_bm25[_with_filter]` | optional qdrant-style payload filter `source=claude,topic~^auth` |
| `memory_timeline` | `MemoryStore::recent_paged` + `count_by_project` | paginated newest-first history; `{items, total}` |
| `memory_profile` | `MemoryStore::projects` + per-project counts | per-project row count and last-seen timestamp |
| `memory_relations` | `MemoryStore::project_edges` BFS | graph traversal from seed ids, depth-bounded |
| `memory_smart_search` | BM25 today, hybrid when embedder attached | unified single-query entry point |
| `memory_export` | `MemoryStore::export_jsonl` | JSON Lines export, one row per line |
| `memory_consolidate` | `MemoryStore::archive_overflow_no_llm` | keep most recent N, archive the rest (LLM-free) |
| `memory_sessions` | `MemoryStore::sessions` / `session_records` | list sessions per project, or rows in one session |
| `memory_set_block` / `memory_get_block` / `memory_list_blocks` | `MemoryStore::*_block` | Letta-style persona / human / context slots |
| `repo_map` | `tree-sitter` signature extraction | Rust / Python / TypeScript signature dump |
| `templates_list` | `rtrt_templates::list_all` | built-in + custom templates |
| `templates_scaffold` | `rtrt_templates::render::{plan,write}` | scaffold from a template |
| `provider_chat` | `Gateway::chat` | multi-provider routing through the bundled gateway |
| `agent_call` | provider invocation bridge | invoke a selected agent target |
| `agent_route` | `select_route` | choose a cost- and headroom-aware agent route |
| `security_scan` | `rtrt_security::run` | scan the pinned project with a named security profile |
| `permission_prompt` | local RTRT permission broker | request a Claude permission decision; defaults to deny |

### MCP auto-capture

`rtrt-mcp` mirrors the dashboard's auto-capture pipeline on every successful `compress` / `compress_ml` / `proxy` / `provider_chat` call. Each invocation runs `redact_secrets` → SHA-256 dedup → `memory.save` → `session_id` tag before returning. The session id is one UUID per process. Same env knobs as the dashboard:

| Env | Default | Effect |
|-----|---------|--------|
| `RTRT_AUTO_CAPTURE` | `1` | Master switch for the MCP auto-capture pipeline |
| `RTRT_AUTO_REDACT` | `1` | Run `redact_secrets` before saving |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | Skip duplicate body hashes seen within N seconds |
| `RTRT_DEFAULT_PROJECT` | current dir name | Project bucket for captured rows |

Local stdio MCP auto-capture resolves linked worktrees to their main Git repository project. On shared HTTP MCP servers, auto-capturing tools accept an explicit `project`; without one, auto-capture is skipped rather than creating or polluting a wrong project.

HTTP transport flags:

- `RTRT_MCP_HTTP_TOKEN` — required bearer token, read only from the environment so it never appears in process arguments; 401 + `WWW-Authenticate` on miss. Constant-time comparison.
- `--allowed-origins host1,host2` / `RTRT_MCP_ALLOWED_ORIGINS` — the RFC 6454 Origin allowlist. Left unset, every request carrying an `Origin` header is rejected with 403; native clients send no `Origin` and are unaffected.
- HTTP startup fails when `RTRT_MCP_HTTP_TOKEN` is missing or empty.

HTTP MCP refuses an empty or missing bearer token. Process and network tools
are unavailable unless their explicit HTTP opt-ins are enabled; filesystem
tools remain bound to the canonical project and cannot be redirected by an
HTTP caller.

For standalone MCP registration, wire it up in `~/.claude.json` (or your agent's MCP config):

```json
{
  "mcpServers": {
    "rtrt": {
      "command": "rtrt-mcp",
      "args": ["--admin", "--memory", "/path/to/memory.sqlite"]
    }
  }
}
```

This standalone registration is separate from OpenCode direct-Claude lanes. OpenCode setup does not read, write, or require `~/.claude.json`; it injects its ephemeral per-invocation settings and strict RTRT MCP config directly.

`rtrt mcp` is a CLI passthrough that forwards `--transport / --bind / --path / --allowed-origins` to the bundled `rtrt-mcp` binary. It inherits `RTRT_MCP_HTTP_TOKEN` from its environment without copying the secret into child-process arguments.

## Dashboard (`rtrt-dashboard`)

```bash
~/.local/bin/rtrt service install --apply
~/.local/bin/rtrt service open
```

The dashboard serves:

| Path | Method | Purpose |
|------|--------|---------|
| `/` | `GET` | Bundled HTML index. Project pages: Overview, Memory, Compression, Command, Statusline, Settings, Templates, Prompts, Diagnose, Security. Tools pages: LLM, Chat, Limits, Environment, Usage, Failover, Connect. |
| `/healthz` | `GET` | Liveness probe (`ok`) |
| `/api/metrics` | `GET` | Gateway summary + recent metrics (drives the SVG sparklines) |
| `/api/budget` | `GET` | `{ cap_usd, spent_usd, remaining_usd }` from the gateway budget meter |
| `/api/prompts` / `/api/prompts/{name}` / `/api/prompts/{name}/{version}` | `GET` | langfuse-style versioned prompts |
| `/api/templates` / `/api/templates/{name}` | `GET` | built-in + custom templates |
| `/api/templates/scaffold` | `POST` | scaffold a project |
| `/api/chat` | `POST` | gateway chat dispatch |
| `/api/compress` | `POST` | rule or ML compressor |
| `/api/proxy` | `POST` | rtrt-proxy filters |
| `/api/diagnose` | `POST` | aider-style failure triage (errors_only + LLM) |
| `/api/memory/save` | `POST` | save memory row with optional metadata |
| `/api/memory/recall` | `POST` | BM25 recall + optional payload filter |
| `/api/memory/blocks` | `GET` / `POST` | Letta blocks listing + upsert |
| `/api/memory/blocks/{name}` | `GET` | single Letta block (project as query param) |
| `/api/repo-map` | `POST` | walk a Rust tree, emit tree-sitter signature map |
| `/api/setup` | `POST` | render an agent MCP config snippet (dry-run only) |

The dashboard accepts only machine invocation `rtrt-dashboard --machine --state-dir <home>/.rtrt/dashboard` and reads its 256-bit hexadecimal bearer from the private `dashboard.env`; environment, argv, URL, and logs never carry the long-lived token. Every `/api/*` route requires constant-time bearer verification except the exact POST-only bootstrap exchange. `/healthz` and bundled SPA assets remain public but never expose the token. Browser API requests must also use an Origin matching the fixed configured bind/loopback authorities, preventing Host-based DNS rebinding; bearer-authenticated API clients without `Origin` remain supported.

`~/.local/bin/rtrt service install --apply` creates or promotes one machine token at exactly `~/.rtrt/dashboard/dashboard.env` (private directory/file). Linux/macOS services invoke `rtrt-dashboard --machine --state-dir ~/.rtrt/dashboard`; no repository cwd, project slug, `RTRT_MEMORY_PATH`, or token argv is used. The dashboard lists verified stores under `~/.rtrt/projects`. **All projects** is an aggregate selector, not a writable project; select a concrete project for project-specific operations. Installation is cwd-independent, idempotent, and prints no secret.

### Failover scope

The Tools **Failover** page manages the `[failover]` policy through `/api/failover/config`. With no project selected, it edits the global policy. With a project selected, an inherited policy is read-only. Choose **Custom** to write a project override, or **Follow global** to remove that override and restore inheritance.

Open the running Linux/macOS dashboard from a trusted terminal with `~/.local/bin/rtrt service open`. It uses a one-time HMAC bootstrap valid for 60 seconds; the long-lived token never enters the URL, opener argv, or logs. If automatic opening is unavailable, `~/.local/bin/rtrt service open --print-bootstrap` prints only the short-lived URL. On Windows, where `rtrt service open` is unsupported, visit <http://127.0.0.1:7311/> and enter the token only in the bootstrap prompt. The SPA erases the fragment after exchange and keeps the bearer only in `sessionStorage`; **Clear API token** removes it for that tab.

## Auto-capture pipeline

The dashboard auto-saves every successful `/api/chat`, `/api/compress`, `/api/diagnose`, and `/api/proxy` request into the memory store. The Claude Code plugin under [`plugins/claude-code/rtrt/`](../plugins/claude-code/rtrt/) does the same for every hook fire across twelve event types: PreToolUse / PostToolUse / PostToolUseFailure / PreCompact / UserPromptSubmit / PostUserPromptSubmit / Notification / Stop / SubagentStart / SubagentStop / SessionStart / SessionEnd. The dashboard's activity feed subscribes to `/api/stream` (Server-Sent Events) for live capture notifications and falls back to 5-second polling if SSE is unavailable.

Every captured event runs through this pipeline:

```
event fires
  ├─ 1. SHA-256 dedup       (5-minute window, configurable)
  ├─ 2. Privacy filter      (AWS / GitHub / OpenAI / Anthropic / Slack /
  │                          Bearer / private-key / api_key=… redacted)
  ├─ 3. Raw save to SQLite  (FTS5 + BM25 auto-indexed)
  ├─ 4. Session id tag      (one UUID per process)
  └─ 5. Optional LLM compress in a background task (off by default)
```

### Configuration

| Env | Default | Effect |
|-----|---------|--------|
| `RTRT_AUTO_CAPTURE` | `1` | Master switch for the dashboard auto-capture pipeline |
| `RTRT_AUTO_REDACT` | `1` | Run `redact_secrets` before saving |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | Skip duplicate body hashes seen within N seconds |
| `RTRT_DEFAULT_PROJECT` | `default` | Project bucket for dashboard captures |
| `RTRT_CONSOLIDATE_INTERVAL_SEC` | `3600` | Hourly archive sweep cadence (0 disables) |
| `RTRT_CONSOLIDATE_KEEP` | `1000` | Rows kept per project after each sweep |
| `RTRT_AUTO_COMPRESS_LLM` | `0` | Opt-in LLM compress daemon; `1` enables |
| `RTRT_AUTO_COMPRESS_MODEL` | `claude-haiku-4-5` | Model id passed to the gateway for each compress call |
| `RTRT_AUTO_COMPRESS_INTERVAL_SEC` | `1800` | Sweep cadence (seconds) |
| `RTRT_AUTO_COMPRESS_AGE_SEC` | `3600` | Only touch rows older than this |
| `RTRT_AUTO_COMPRESS_MIN_CHARS` | `512` | Skip rows shorter than this |
| `RTRT_AUTO_COMPRESS_BATCH` | `20` | Max rows compressed per project per sweep |
| `RTRT_AUTO_COMPRESS_MAX_TOKENS` | `512` | Max output tokens per compress call |

Rows the LLM compress daemon rewrites get tagged with `metadata.compressed_at`, `compressed_model`, `compressed_from_chars`, and `compressed_to_chars`. If the LLM's output is empty or no shorter than the input, the row is left untouched but `compressed_skip=no-shrink` is recorded so the daemon does not keep retrying it. Embeddings are intentionally not regenerated — recall stays serviceable off the BM25 index that `set_body` updates in lockstep.

**Local model choice.** The default `claude-haiku-4-5` targets a cloud key. For a fully local setup against Ollama / an OpenAI-compatible endpoint, set `RTRT_AUTO_COMPRESS_MODEL=gemma3:4b` — it's the best local compressor in our sweep (robust across every length, fits a modest GPU). See the model comparison table in [`docs/PERF.md`](PERF.md#llm-auto-compress--local-model-sweep--2026-05-26). Avoid `granite4.1:8b` for this (fails on very long captures) and `llama3.1:8b` (corrupts facts).

## ONNX token-importance backend (opt-in)

Build with `--features onnx` to swap the heuristic `MlCompressor` for a real LLMLingua-2-style scorer:

```bash
cargo build --release -p rtrt-cli --features onnx
rtrt compress --ml --ratio 0.5 \
    --onnx-model     ~/.rtrt/models/llmlingua2.onnx \
    --onnx-tokenizer ~/.rtrt/models/tokenizer.json \
    < verbose.md
```

The two files are not shipped with RTRT — supply them yourself. The model contract is documented in `crates/rtrt-compress/src/ml_onnx.rs` (named inputs `input_ids` + `attention_mask` of shape `[1, seq_len]`, output `[1, seq_len, 2]` per-token keep-probability or `[1, seq_len]` saliency). `ort` is configured with `load-dynamic`, so the ONNX Runtime shared library is resolved at startup; install it system-wide (`libonnxruntime.so` / `onnxruntime.dll`) or set `ORT_DYLIB_PATH`.

## BERTScore quality check (opt-in)

`rtrt-eval` ships a BERTScore evaluator behind the `bertscore` feature. Pass any BERT-like ONNX encoder + the matching `tokenizer.json` and it scores every fixture sample against its `Compressor::compress` output:

```bash
cargo run --release -p rtrt-eval --features bertscore -- bertscore \
    --model     ~/.rtrt/models/bert-mini.onnx \
    --tokenizer ~/.rtrt/models/tokenizer.json \
    --level full
```

Output is one line per sample (precision, recall, F1) plus a mean row. The encoder must emit `[1, seq_len, hidden]`; the score is greedy cosine alignment between subword embeddings (special tokens skipped). Drop in a real labelled corpus via `--fixture path/to/dataset.json` (same shape as the built-in smoke fixture) to publish the trustworthy numbers the long-term quality targets in `docs/PERF.md` are written against.

### Plugin install

```bash
# Copy the plugin into Claude Code's plugin cache.
mkdir -p ~/.claude/plugins/cache/rtrt
cp -R plugins/claude-code/rtrt/* ~/.claude/plugins/cache/rtrt/
chmod +x ~/.claude/plugins/cache/rtrt/hooks/*.sh

# Enable in your Claude Code settings.json:
#   "plugins": ["rtrt"]
```

The hooks write via `rtrt` CLI when it is on `PATH` (`RTRT_BIN` overrides), or fall back to `POST /api/memory/save` when `RTRT_DASHBOARD_URL` is set (with `RTRT_DASHBOARD_TOKEN` for the bearer header).

### Live activity stream

`GET /api/stream` is a Server-Sent Events endpoint. Every successful auto-capture pushes a JSON event:

```json
{"type":"memory.save","id":42,"kind":"post-tool-use","project":"rtrt","session":"..."}
```

Subscribe with any SSE client (curl `--no-buffer` works) to drive a live feed without polling `/api/metrics`.

### Token usage

`GET /api/tokens/summary` aggregates the gateway's request history into hourly and daily buckets — `{hour_ts, calls, input_tokens, output_tokens}` and the daily equivalent. Use it to plot spend or to set alarms.

### Consolidation

The hourly daemon runs `archive_overflow_no_llm` on every project that exceeds `RTRT_CONSOLIDATE_KEEP`. Oldest rows are dropped, newer rows stay. Manual control:

```bash
# CLI: keep last 20 rows, drop the rest (writes a single summary row).
rtrt memory compress --project rtrt --keep 20 --provider openai-compat \
   --base-url http://127.0.0.1:11434/v1 --model llama3.2

# MCP: same operation, LLM-free.
# memory_consolidate { project: "rtrt", keep: 20 }
```

## Configuration file

Configuration is two-tier:

1. **Global** — `~/.rtrt/config.toml` (override the path with `RTRT_CONFIG`). Create it with `rtrt config init`; inspect the resolved path with `rtrt config path`. The base kernel — hooks, MCP wiring, statusline command binding — lives here and is managed by `rtrt setup`.
2. **Per-project** — `<repo>/.rtrt/config.toml`. Optional overrides only: output level (`off` / `lite` / `full` / `ultra`), compression, per-project agent + provider enablement, statusline, and Failover. Absent fields inherit the global value; the effective config is global ⊕ project. When every override is back at "follow global", the file is deleted so the repo stays clean. The dashboard edits this layer through its **Follow global / Custom** scope toggles.

Selected global sections:

```toml
# Optional daily usage ceilings per routing target — powers `rtrt usage`
# headroom and the router's headroom-weighted selection. Targets without an
# entry report no cap (never fabricated).
[limits.openai]
daily_tokens = 1_000_000
daily_requests = 2_000

[limits.ollama]
daily_tokens = 250_000
```

See `crates/rtrt-core/src/config.rs` for the full schema (`[compression]`, `[memory]`, `[dashboard]`, `[providers]`, `[agents]`, `[capture]`, `[auto_compress]`, `[embeddings]`, `[security]`, `[limits]`, `[[projects]]`).
