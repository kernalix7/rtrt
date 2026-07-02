# Features

**English** | [한국어](FEATURES.ko.md)

This page covers the implementation details of each token-reduction surface and the scaffolding feature.

## Output compression

`rtrt-compress` is a regex-based rewriter that runs in three phases:

1. **Redact phase** — secret-shaped substrings (AWS access keys, GitHub PATs, OpenAI / Anthropic / Slack tokens, Bearer auth headers, generic `api_key=…` patterns, and PEM private-key blocks) are replaced with `<REDACTED:<kind>>` markers so they never reach the rule pass or any downstream LLM.
2. **Stash phase** — `PROTECT` matches code fences (` ``` `), inline code (`` ` ``), `https?://…` URLs, and `"…"` quoted strings and replaces each with an opaque placeholder (`\u{0001}RTRT_PROTECT_<n>\u{0002}`). The original text is stored in a slot table.
3. **Rule phase** — a level-dependent ordered rule set applies `Regex::replace_all`. The level controls which rule classes run:
   - `lite` — fillers + multi-space + multi-newline collapse.
   - `full` — `lite` + pleasantries + hedging (`I think`, `perhaps`, …) + discourse markers (`moreover`, `however`, …) + meta-phrases (`it is important to note that`, …).
   - `ultra` — `full` + articles (`a` / `an` / `the`) + phrase shortening (`due to the fact that` → `because`, `in order to` → `to`, `a number of` → `several`, `for instance` → `e.g.`, etc.).
   - `extreme` — `ultra` + verbose qualifiers (`very`, `extremely`, `quite`, `rather`, …).
4. **Restore phase** — placeholders are swapped back for their original text.

The protection list is intentionally conservative — anything that could be technical content (code, URLs, errors) is preserved verbatim.

API:

```rust
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

let c = Compressor::new(CompressionLevel::Ultra);
let out = c.compress("I think the bug is, perhaps, in the parser.");
// out: "bug is, in parser."
```

### Compression savings

Measured by `scripts/bench.sh` over the fixtures in `crates/rtrt-compress/benches/fixtures/`. Numbers are char-reduction percentages; lower is more conservative.

| Fixture | `lite` | `full` | `ultra` | `extreme` |
|---------|-------:|-------:|--------:|----------:|
| `short` (conversational AI reply) |  6% | 25% | **32%** | 32% |
| `mixed` (prose + occasional code) |  3% | 12% | 18% | **19%** |
| `long`  (multi-paragraph explainer) |  2% | 10% | **15%** | 15% |
| `code`  (code-heavy response) |  2% |  3% |  6% | 6% |

What rule-based passes can and can't do:

- **Can**: drop fillers, pleasantries, hedging, discourse markers, articles, verbose qualifiers, and re-express common verbose phrases.
- **Can't**: reach caveman's published 60-75% on natural prose without an LLM in the loop — those numbers come from the LLM agreeing to *generate* terse text up front, not from post-hoc deletion.

For caveman-class numbers, use the LLM mode (`llm-compress` feature). [`LlmCompressor`](https://docs.rs/rtrt-compress/latest/rtrt_compress/struct.LlmCompressor.html) routes through any `Provider` — including a local Ollama server — and asks the model to rewrite the passage. Same idea as caveman; works on existing strings instead of requiring the agent to be in caveman mode from the start.

```bash
# local Ollama, free, offline after first model pull
ollama pull llama3.2
echo "I think the bug is, perhaps, in the parser..." | rtrt compress --llm \
  --provider openai-compat --base-url http://127.0.0.1:11434/v1 --model llama3.2

# cloud Anthropic
ANTHROPIC_API_KEY=... rtrt compress --llm \
  --provider anthropic --model claude-haiku-4-5 < passage.md
```

### Tree-sitter signature extraction (the `treesitter` feature)

For code-heavy responses, `SignatureExtractor` walks the parsed AST and emits only the top-level signatures — `fn` headers, `struct` / `enum` / `trait` / `type` / `const` declarations, and `impl` block headers with method signatures inside — replacing every function body with `{ /* body */ }`.

```bash
rtrt signatures --lang rust < src/anthropic.rs
# typical: 70–80% byte savings on a normal Rust source file
```

Current grammar: Rust (`tree-sitter-rust`). Adding another language is a matter of enabling the matching grammar crate and extending [`Language`](https://docs.rs/rtrt-compress/latest/rtrt_compress/enum.Language.html). The CLI binary builds with `treesitter` enabled by default; library users pulling `rtrt-compress` directly opt in via `features = ["treesitter"]`.

### Secret redaction

The redactor runs **before** the rule pass, so secrets are scrubbed even if compression is set to `lite`. Patterns covered:

- `aws-access-key`: `AKIA…` / `ASIA…` 20-char keys.
- `aws-secret`: `aws_secret_access_key=…` 40-char base64.
- `github-pat`: `ghp_…` 40-char PAT.
- `github-token`: `gh[opsur]_…` (fine-grained tokens, etc.).
- `openai-key`: `sk-…` / `sk-proj-…`.
- `anthropic-key`: `sk-ant-…`.
- `slack-token`: `xox[abprs]-…`.
- `bearer-token`: `Authorization: Bearer …`.
- `private-key`: `-----BEGIN … PRIVATE KEY-----` blocks.
- `generic-api-key`: `api_key=…` / `apikey=…` (context-required).

Each match becomes `<REDACTED:<kind>>`. Idempotent — re-running on already-redacted text is a no-op.

## Command-output filtering

`rtrt-proxy` ships a small dispatch table. Each `CommandFilter` has a `command` prefix and an `apply` function that takes raw stdout and returns filtered stdout.

Currently shipped filters:

| Command prefix | Strategy |
|----------------|----------|
| `git status` | drop `On branch …`, `Your branch …`, `(use …)`, `nothing to commit …` lines; collapse blank lines |
| `git log` | drop `Author:` / `Date:` lines; collapse blank lines |
| `cargo build` | drop `Compiling …`, `Finished …`, `Downloading …`, `Downloaded …` lines |
| `cargo test` | same as `cargo build` |

`filter_for("<command>")` returns the first matching filter. Unmatched commands pass through unchanged.

## Persistent memory

`rtrt-memory` opens a SQLite database (default `.rtrt/memory.sqlite`) and runs the migration on first open. The schema is:

```sql
CREATE TABLE memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    body        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_memories_project ON memories(project);

CREATE VIRTUAL TABLE memories_fts
    USING fts5(body, content='memories', content_rowid='id');

CREATE TABLE embeddings (
    memory_id   INTEGER PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
    model       TEXT NOT NULL,
    vector      BLOB NOT NULL
);

CREATE TABLE edges (
    src_id      INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    dst_id      INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation    TEXT NOT NULL,
    PRIMARY KEY (src_id, dst_id, relation)
);
```

BM25 recall against `memories_fts`:

```rust
let store = MemoryStore::open(".rtrt/memory.sqlite")?;
store.save("my-project", "note", "Rust is a systems language")?;
let hits = store.recall_bm25("my-project", "rust", 5)?;
```

Vector and hybrid recall require an `Embedder`. The default is `all-MiniLM-L6-v2` via [`fastembed`](https://crates.io/crates/fastembed), 384-dim, ONNX, offline after first download. The feature is gated:

```toml
[dependencies]
rtrt-memory = { version = "0.2", features = ["embeddings"] }
```

Usage:

```rust
use rtrt_memory::{MemoryStore, FastEmbedder};

let store = MemoryStore::open(".rtrt/memory.sqlite")?;
let embedder = FastEmbedder::new_default()?;
store.save_embedded("my-project", "note", "Rust is a systems language", &embedder)?;
let hits = store.recall_hybrid("my-project", "rust toolchain", 5, &embedder)?;
```

Recall details:

- **`recall_bm25`** — FTS5 ranked by built-in BM25; project-scoped; no embedder required.
- **`recall_vector`** — embeds the query, scores every project memory by cosine similarity, sorts in process. Linear in stored embeddings; this will be replaced by an HNSW index when scale demands.
- **`recall_hybrid`** — Reciprocal Rank Fusion of BM25 + vector with `rrf_k = 60`. Each stream is fetched at `limit * 2` so single-stream-only matches still surface.

The `edges` table is reserved for graph traversal.

**First-use note**: `FastEmbedder::new_default()` downloads the model (~90 MB) to fastembed's cache dir on first construction. Subsequent uses are offline.

### LLM-backed extract + compress (the `llm` feature)

Two memory operations need an LLM rather than just an embedder:

- **Extract** — turn a long passage into a list of atomic facts, one row per fact. Used at ingestion to avoid storing pre-chewed prose.
- **Compress** — collapse the oldest memories in a project into a single archival summary, then delete the originals. Used when the working pool gets large.

Both flow through the [`Summariser`](https://docs.rs/rtrt-memory/latest/rtrt_memory/summarise/trait.Summariser.html) trait. The shipped implementation, `LlmSummariser`, wraps any `rtrt_providers::Provider`, so the same code works against Anthropic, OpenAI, or any OpenAI-compatible local endpoint.

#### Local LLM via Ollama (recommended for free / offline use)

Ollama exposes a `/v1/chat/completions` endpoint that matches the OpenAI wire format. No new adapter is needed — RTRT's existing `OpenAICompatibleProvider` works out of the box:

```bash
# one-time setup
ollama pull llama3.2          # or qwen2.5:7b, gemma2:9b, etc.
ollama serve                  # binds 127.0.0.1:11434 by default

# extract atomic facts from a long passage into project "p1"
echo "Long passage about RTRT architecture..." | rtrt memory extract \
  --project p1 \
  --provider openai-compat \
  --base-url http://127.0.0.1:11434/v1 \
  --model llama3.2

# compress: keep most-recent 20, summarise the rest
rtrt memory compress \
  --project p1 \
  --keep 20 \
  --provider openai-compat \
  --base-url http://127.0.0.1:11434/v1 \
  --model llama3.2
```

#### Cloud LLM (Anthropic / OpenAI)

```bash
ANTHROPIC_API_KEY=... rtrt memory extract \
  --project p1 \
  --provider anthropic \
  --model claude-haiku-4-5 \
  < passage.txt

OPENAI_API_KEY=... rtrt memory compress \
  --project p1 --keep 10 \
  --provider openai --model gpt-5.4-mini
```

The CLI commands route to `MemoryStore::extract_and_save` and `MemoryStore::compress_project` from the library API.

## Multi-provider routing

`rtrt-providers` defines a `Provider` trait:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn supported_models(&self) -> &[&'static str];
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;
}
```

Built-in adapters:

- `AnthropicProvider` — base URL `https://api.anthropic.com/v1`. Models: `claude-opus-4-7`, `claude-sonnet-4-6`, `claude-haiku-4-5`.
- `OpenAIProvider` — base URL `https://api.openai.com/v1`. Models: `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex-spark`.
- `OpenAICompatibleProvider` — user-provided base URL. Targets Ollama, llama.cpp server, vLLM, LM Studio, and any other OpenAI-compatible HTTP endpoint.

`chat` and `chat_stream` are implemented against the real HTTP APIs for all three adapters; a `Gateway` fronts every registered provider with per-request metrics, an optional budget cap, a response cache, and retry / fallback.

### Usage ledger + windowed headroom (`rtrt usage`)

Every provider invocation appends one row to a local usage ledger at `~/.rtrt/provider-usage.tsv` (override with `RTRT_PROVIDER_USAGE_PATH`):

```text
epoch_ts \t target \t model \t input_tokens \t output_tokens \t est \t ok
```

- The file is capped at the most-recent 5000 rows; writes are best-effort and never fail the invocation.
- CLI shell-outs return only text, so their token counts are estimates (~chars/4) and stay marked `est` end to end — estimated rows render with `~`.
- Usage is bucketed into rolling **5h / 24h / 7d** windows per target; `rtrt usage` prints the table (`--format json` available).
- **Headroom** compares the 24h window against optional daily caps in `~/.rtrt/config.toml`:

```toml
[limits.openai]
daily_tokens = 1_000_000
daily_requests = 2_000

[limits.ollama]
daily_tokens = 250_000
```

Targets without a `[limits]` entry report no cap — RTRT never fabricates a ceiling.

### Headroom-weighted route selection + automatic failover (`rtrt route`)

`rtrt route` picks the cheapest useful target for a prompt. Ranking is cost-tier first — local-free → subscription-flat → API-metered → unknown — then headroom-aware inside each tier:

- A candidate whose scarcest limited dimension (tokens or requests) is under **~15% remaining** is penalized within its cost tier.
- A fully **exhausted** target (0 remaining on any dimension) sinks below every other candidate and is only ever a last-resort fallback.
- Ties break toward the target with the larger remaining headroom fraction.

`rtrt route --explain` prints the decision, the ranked alternatives, and the usage / headroom that drove it; `--dry-run` prints the decision without invoking.

With `--failover`, `rtrt route` (and `rtrt call --failover`) walks the ranked candidate list: a **retryable** failure (rate-limit / quota / 429 / 5xx / timeout) falls over to the next ranked target, a terminal error stops the walk, and the result summarises which targets fell over and why.

The dashboard mirrors this with `GET /api/usage` (windowed usage + headroom per target) and `GET /api/route/preview` (the load-balancing decision for the *next* request, no prompt required), rendered as usage / headroom gauges and a routing preview on the Tools side.

## Security & license scanning

`rtrt-security` is a profile-driven scanner for AI-generated artifacts, modeled on OpenSCAP-style declarative profiles. Five engines run per scan:

| Engine | What it checks |
|--------|----------------|
| `secrets` | built-in secret patterns + Shannon-entropy gate; excerpts are redacted |
| `licenses` | SPDX manifest policy (allow / forbid lists), optional header check, workspace-inheritance aware |
| `deps` | Cargo.lock / package-lock hygiene (git / wildcard / yanked) + optional offline RustSec advisory match |
| `patterns` | regex source scanner with language + path filters |
| `ai` | AI-artifact checks: hallucinated-import / slopsquatting, base64 blobs, eval usage, TODO-secret, unsafe blocks |

Six built-in profiles (`ai-default`, `ai-strict`, `owasp-top-10`, `asvs-l2`, `cis-baseline`, `nist-ssdf`) map every rule to industry standards (CWE / OWASP / NIST / CIS / SLSA / EU AI Act). Profiles are declarative TOML — drop your own under `~/.rtrt/security/profiles/` to override built-ins or add new ones.

```bash
rtrt security scan --profile ai-default [--path DIR] [--json]
rtrt security profile list
rtrt security profile show ai-strict
rtrt security gate --profile ai-default        # non-zero exit at/above threshold — CI gate
rtrt security init                             # copy built-ins to ~/.rtrt/security/profiles/
```

The same scanner backs the dashboard Security page (project-aware: scans the selected project's path with its bound profile) and the MCP `security_scan` tool.

## Two-tier configuration & project lifecycle

Configuration is layered:

1. **Global base kernel** — `~/.rtrt/config.toml` plus the agent wiring (hooks / MCP / statusline command binding). Managed by `rtrt setup`; projects never override it.
2. **Per-project overrides** — `<repo>/.rtrt/config.toml` (`ProjectConfig`): optional output level (`off` / `lite` / `full` / `ultra`), compression, per-project agent + provider enablement, and statusline. Absent fields inherit the global value; effective config = global ⊕ project (`Config::load_effective`). An all-default override file is deleted so the repo stays clean.

The dashboard exposes each per-project surface behind a **Follow global / Custom** scope toggle.

Lifecycle commands:

```bash
rtrt migrate [--path DIR] [--apply]      # migrate an existing repo to the rtrt project standard (dry-run by default)
rtrt project refresh [--apply]           # one-command alias: render contract → activate canonical settings → audit
rtrt project status | health | repair    # inspect / verify / repair the standardization contract
```

`rtrt migrate` / `rtrt project refresh` also detect project-level rtrt-owned key shadows (e.g. a project `.claude/settings.json` re-declaring `statusLine`) and strip them with a `.bak` backup so the project defers to the global base kernel.

## Project scaffolds

`rtrt-templates` ships six built-in templates programmatically (no external file embedding required). Each template is a `Template { name, description, source, variables, files, post_hooks }`.

Built-ins:

| Name | What you get |
|------|--------------|
| `rust-cli` | Rust binary with `clap` + `anyhow` + `tracing`; `git init` post-hook |
| `rust-lib` | Rust library with a `add` example test |
| `rust-axum` | Rust HTTP service with `axum` + `tokio` + `tracing-subscriber` |
| `node-typescript` | ESM TypeScript project with `tsx`; `npm install` post-hook |
| `python-uv` | `pyproject.toml` project laid out for `uv sync` |
| `go-cli` | Minimal Go CLI with `go.mod`; `go mod tidy` post-hook |

Shared variables:

- `project_name` (required)
- `author` (default `Unknown`)
- `license` (default `MIT`)

Variable substitution uses `{{key}}`. Paths support substitution too — `src/{{project_name}}/__init__.py` becomes `src/hello/__init__.py`.

### Custom templates

```
~/.rtrt/templates/
└── my-template/
    ├── manifest.toml
    ├── Cargo.toml.tmpl
    └── src/main.rs.tmpl
```

`manifest.toml` shape:

```toml
name = "my-template"
description = "My custom Rust scaffold"
post_hooks = ["git init"]

[[variables]]
name = "project_name"
description = "Project name"
required = true

[[files]]
path = "Cargo.toml"
source = "Cargo.toml.tmpl"   # or use inline `content = "..."`

[[files]]
path = "src/main.rs"
source = "src/main.rs.tmpl"
```

Each `[[files]]` entry either points to a `source` file (relative to the manifest dir) or carries inline `content`. Both apply variable substitution.

## MCP and dashboard

`rtrt-mcp` is a real rmcp-based MCP server over stdio and Streamable HTTP, exposing the memory / compress / proxy / templates / provider / security tool surface (see [USAGE.md](USAGE.md) for the full tool table).

`rtrt-dashboard` is an axum server bound to `127.0.0.1:7311` by default. It serves:

- `/` — minimal HTML with the savings stats and template gallery.
- `/api/stats` — JSON savings.
- `/api/templates` — JSON template list.
- `/api/templates/{name}` — full template manifest.
- `/api/templates/scaffold` — POST endpoint to scaffold from the browser.

The scaffold endpoint accepts the same `{ template, target, variables, overwrite }` shape as the CLI `rtrt new` command.
