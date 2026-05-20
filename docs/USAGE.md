# Usage

**English** | [한국어](USAGE.ko.md)

This page documents the `rtrt` CLI, the `rtrt-mcp` server, and the `rtrt-dashboard` web UI as of v0.1.0.

## CLI

```text
rtrt --help
```

### `rtrt compress`

Compress text read from stdin and write to stdout.

```bash
echo "Sure, I'd be happy to help. The bug is really in the parser." \
  | rtrt compress -l ultra
```

Flags:

- `-l, --level <lite|full|ultra>` — compression intensity. Default `full`.

Rules:

- `lite` — drop filler words (`just`, `really`, `basically`, …) and collapse multi-spaces.
- `full` — `lite` plus pleasantries (`sure`, `certainly`, `happy to`, …).
- `ultra` — `full` plus articles (`a`, `an`, `the`).

Code blocks (` ``` ` and ` ` `), URLs, and `"quoted strings"` are stashed before the rule pass and restored afterwards, so technical content is never rewritten.

### `rtrt proxy`

Filter a command's stdout for a known command name.

```bash
git status | rtrt proxy "git status"
cargo build 2>&1 | rtrt proxy "cargo build"
```

Built-in filter rules cover `git status`, `git log`, `cargo build`, `cargo test`. When the command does not match a built-in, output passes through unchanged.

### `rtrt templates`

List available templates (built-in + custom).

```text
rust-cli           [BuiltIn]  Rust binary crate with clap + anyhow + tracing
rust-lib           [BuiltIn]  Rust library crate with criterion benches
rust-axum          [BuiltIn]  Rust HTTP service with axum + tokio + tracing
node-typescript    [BuiltIn]  Node.js TypeScript project (ESM, tsx runner)
python-uv          [BuiltIn]  Python project managed with uv (pyproject.toml)
go-cli             [BuiltIn]  Go CLI with cobra + standard layout
```

Custom templates live in `~/.rtrt/templates/<name>/manifest.toml` and appear under `[Custom]`.

### `rtrt new`

Scaffold a project from a template.

```bash
rtrt new rust-cli ./hello \
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

## MCP server (`rtrt-mcp`)

```bash
# stdio (default; what Claude Code / Codex / Cursor / Windsurf use)
rtrt-mcp --memory ~/.rtrt/memory.sqlite
```

Implemented via [`rmcp`](https://crates.io/crates/rmcp), the official Rust MCP SDK. Tools shipped in v0.2:

| Tool | Wraps | Notes |
|------|-------|-------|
| `compress` | `Compressor::compress` | accepts `level = lite \| full \| ultra` (default `full`) |
| `memory_save` | `MemoryStore::save` | inserts into FTS5 and the BM25 index |
| `memory_recall` | `MemoryStore::recall_bm25` | project-scoped, BM25 ranking |
| `templates_list` | `rtrt_templates::list_all` | enumerates built-in + custom templates |
| `templates_scaffold` | `rtrt_templates::render::{plan,write}` | scaffolds from a template |

Wire it up in `~/.claude.json` (or your agent's MCP config):

```json
{
  "mcpServers": {
    "rtrt": {
      "command": "rtrt-mcp",
      "args": ["--memory", "/path/to/memory.sqlite"]
    }
  }
}
```

HTTP/SSE transport, plus `provider_chat` and the LLM-backed `memory_extract` / `memory_compress` tools, are on the v0.3 roadmap.

## Dashboard (`rtrt-dashboard`)

```text
RTRT_DASHBOARD_BIND=127.0.0.1:3111 rtrt-dashboard
```

The dashboard serves:

| Path | Method | Purpose |
|------|--------|---------|
| `/` | `GET` | Minimal HTML index — token-savings stats + template gallery |
| `/healthz` | `GET` | Liveness probe (`ok`) |
| `/api/stats` | `GET` | JSON: input / output tokens saved, active provider |
| `/api/templates` | `GET` | JSON: list of templates (built-in + custom) |
| `/api/templates/{name}` | `GET` | JSON: full template manifest |
| `/api/templates/scaffold` | `POST` | Scaffold a project — JSON body `{ template, target, variables, overwrite }` |

The dashboard binds `127.0.0.1` by default. Override with `RTRT_DASHBOARD_BIND`.

## Configuration file

Planned (`~/.rtrt/config.toml`). See `crates/rtrt-core/src/config.rs` for the schema; `Config::default()` is the only currently-supported loader.
