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
rtrt mcp --transport http --bind 127.0.0.1:7312 \
  --http-token "$RTRT_MCP_HTTP_TOKEN" \
  --allowed-origins https://app.example.com
```

### `rtrt benchmark`

Wrap `cargo bench` so the published 60%+ savings claim is one command away.

```bash
rtrt benchmark                    # cargo bench -p rtrt-compress --bench compress_bench
rtrt benchmark --extra '--quick'
```

## MCP server (`rtrt-mcp`)

```bash
# stdio (default; what Claude Code / Codex / Cursor / Windsurf use)
rtrt-mcp --memory ~/.rtrt/memory.sqlite

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
| `memory_set_block` / `memory_get_block` / `memory_list_blocks` | `MemoryStore::*_block` | Letta-style persona / human / context slots |
| `templates_list` | `rtrt_templates::list_all` | built-in + custom templates |
| `templates_scaffold` | `rtrt_templates::render::{plan,write}` | scaffold from a template |
| `provider_chat` | `Gateway::chat` | multi-provider routing through the bundled gateway |

HTTP transport flags:

- `--http-token <T>` / `RTRT_MCP_HTTP_TOKEN` — required bearer token; 401 + `WWW-Authenticate` on miss. Constant-time comparison.
- `--allowed-origins host1,host2` / `RTRT_MCP_ALLOWED_ORIGINS` — pluck into `StreamableHttpServerConfig.allowed_origins` for RFC 6454 Origin validation.
- Non-loopback bind without a token logs a startup warning.

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

`rtrt mcp` is a CLI passthrough that forwards `--transport / --bind / --path / --http-token / --allowed-origins` to the bundled `rtrt-mcp` binary.

## Dashboard (`rtrt-dashboard`)

```text
RTRT_DASHBOARD_BIND=127.0.0.1:7311 \
  RTRT_DASHBOARD_TOKEN=$(openssl rand -hex 16) \
  rtrt-dashboard
```

The dashboard serves:

| Path | Method | Purpose |
|------|--------|---------|
| `/` | `GET` | Bundled HTML index — Metrics / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup tabs |
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

All `/api/*` routes are gated by a bearer-token middleware when `RTRT_DASHBOARD_TOKEN` is set; the bundled HTML index and `/healthz` stay open. Non-loopback bind without a token logs a startup warning.

## Configuration file

Planned (`~/.rtrt/config.toml`). See `crates/rtrt-core/src/config.rs` for the schema; `Config::default()` is the only currently-supported loader.
