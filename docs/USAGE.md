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
# rule-based (default)
echo "Sure, I'd be happy to help. The bug is really in the parser." \
  | rtrt compress -l ultra

# LLM-backed (any provider; Ollama shown)
echo "I think the bug is, perhaps, in the parser..." | rtrt compress --llm \
  --provider openai-compat --base-url http://127.0.0.1:11434/v1 --model llama3.2
```

Flags:

- `-l, --level <lite|full|ultra|extreme>` — compression intensity. Default `full`.

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

Implemented via [`rmcp`](https://crates.io/crates/rmcp), the official Rust MCP SDK. Tools currently shipped:

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

HTTP/SSE transport and LLM-backed `memory_extract` / `memory_compress` tools remain on the roadmap.

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
