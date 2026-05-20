# Features

**English** | [한국어](FEATURES.ko.md)

This page covers the implementation details of each token-reduction surface and the scaffolding feature.

## Output compression

`rtrt-compress` is a regex-based rewriter. It runs in two phases:

1. **Stash phase** — `PROTECT` matches code fences (` ``` `), inline code (`` ` ``), `https?://…` URLs, and `"…"` quoted strings and replaces each with an opaque placeholder (`\u{0001}RTRT_PROTECT_<n>\u{0002}`). The original text is stored in a slot table.
2. **Rule phase** — a level-dependent ordered rule set applies `Regex::replace_all`. The level controls which rule classes run:
   - `lite` — fillers + multi-space collapse.
   - `full` — `lite` plus pleasantries.
   - `ultra` — `full` plus articles.
3. **Restore phase** — placeholders are swapped back for their original text.

The protection list is intentionally conservative — anything that could be technical content (code, URLs, errors) is preserved verbatim.

API:

```rust
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

let c = Compressor::new(CompressionLevel::Ultra);
let out = c.compress("the bug is `really` in the parser");
// out: "bug is `really` in parser"
```

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
