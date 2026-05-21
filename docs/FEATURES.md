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
- **`recall_vector`** — embeds the query, scores every project memory by cosine similarity, sorts in process. Linear in stored embeddings; v0.3 swaps this for an HNSW index.
- **`recall_hybrid`** — Reciprocal Rank Fusion of BM25 + vector with `rrf_k = 60`. Each stream is fetched at `limit * 2` so single-stream-only matches still surface.

The `edges` table is reserved for v0.3 graph traversal.

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

v0.1.0 `chat` implementations return `Error::Provider("... not implemented yet")`. Wiring real chat is roadmap item.

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

`rtrt-mcp` is currently a stub that announces the planned tool surface (`compress`, `memory.save`, `memory.recall`, `provider.chat`). The stdio transport implementation is on the roadmap.

`rtrt-dashboard` is an axum server bound to `127.0.0.1:3111` by default. It serves:

- `/` — minimal HTML with the savings stats and template gallery.
- `/api/stats` — JSON savings.
- `/api/templates` — JSON template list.
- `/api/templates/{name}` — full template manifest.
- `/api/templates/scaffold` — POST endpoint to scaffold from the browser.

The scaffold endpoint accepts the same `{ template, target, variables, overwrite }` shape as the CLI `rtrt new` command.
