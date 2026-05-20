# Architecture

**English** | [한국어](ARCHITECTURE.ko.md)

## Diagram

```
+--------------------+      +--------------------+      +--------------------+
|     rtrt-cli       |----->|     rtrt-mcp       |      |  rtrt-dashboard    |
|  (one-line entry)  |      |  (MCP stdio/HTTP)  |      |     (axum web)     |
+---------+----------+      +----------+---------+      +----------+---------+
          |                            |                           |
          v                            v                           v
+----------------------------------------------------------------------------+
|                                rtrt-core                                   |
|     plugin trait · config · errors · token accounting · telemetry          |
+----------------------------------------------------------------------------+
   |              |               |                |                 |
   v              v               v                v                 v
+--------+   +---------+    +----------+    +-------------+    +---------+
| rtrt-  |   | rtrt-   |    | rtrt-    |    | rtrt-       |    | rtrt-   |
| compr. |   | proxy   |    | memory   |    | providers   |    | templ.  |
+--------+   +---------+    +----------+    +-------------+    +---------+
```

## Crate boundaries

| Crate | Public API | Depends on |
|-------|------------|------------|
| `rtrt-core` | `Error`, `Result`, `CompressionLevel`, `TokenCount`, `TokenStats`, `Plugin`, `PluginKind`, `PluginMetadata`, `Config` | `serde`, `serde_json`, `thiserror`, `async-trait` |
| `rtrt-compress` | `Compressor::new`, `Compressor::compress` | `rtrt-core`, `regex`, `once_cell` |
| `rtrt-proxy` | `filter_for`, `CommandFilter`, `FILTERS` | `rtrt-core`, `regex`, `once_cell` |
| `rtrt-memory` | `MemoryStore::open`, `MemoryStore::open_in_memory`, `MemoryStore::save`, `MemoryStore::recall_bm25`, `MemoryRecord` | `rtrt-core`, `rusqlite` (bundled), `serde`, `serde_json`, `tokio`, `tracing` |
| `rtrt-providers` | `Provider`, `ChatMessage`, `ChatRequest`, `ChatResponse`, `Role`, `AnthropicProvider`, `OpenAIProvider`, `OpenAICompatibleProvider` | `rtrt-core`, `reqwest`, `serde`, `serde_json`, `tokio`, `async-trait`, `tracing` |
| `rtrt-templates` | `Template`, `TemplateFile`, `TemplateVariable`, `RenderPlan`, `RenderedFile`, `builtin::ALL`, `custom::scan_default_dir`, `render::plan`, `render::write`, `list_all`, `find` | `rtrt-core`, `toml`, `walkdir`, `dirs`, `once_cell`, `serde`, `serde_json` |
| `rtrt-mcp` | bin `rtrt-mcp` | `rtrt-core`, `rtrt-compress`, `rtrt-memory`, `rtrt-providers`, `tokio`, `tracing-subscriber` |
| `rtrt-dashboard` | bin `rtrt-dashboard` | `rtrt-core`, `rtrt-templates`, `axum`, `tower`, `tower-http`, `tokio`, `tracing-subscriber` |
| `rtrt-cli` | bin `rtrt` | every other crate, `clap`, `tokio`, `tracing-subscriber` |

## Source tree

```
.
├── Cargo.toml                       # workspace + shared deps + profiles
├── Cargo.lock
├── rust-toolchain.toml              # pinned to stable
├── rustfmt.toml
├── .gitignore
├── LICENSE                          # MIT
├── README.md
├── CHANGELOG.md
├── CONTRIBUTING.md
├── CODE_OF_CONDUCT.md
├── SECURITY.md
├── THIRD_PARTY_LICENSES.md
├── .github/
│   ├── FUNDING.yml
│   ├── ISSUE_TEMPLATE/
│   │   ├── bug_report.md
│   │   └── feature_request.md
│   ├── PULL_REQUEST_TEMPLATE.md
│   └── workflows/
│       └── ci.yml
├── crates/
│   ├── rtrt-core/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── config.rs
│   │       ├── error.rs
│   │       ├── plugin.rs
│   │       └── token.rs
│   ├── rtrt-compress/src/lib.rs
│   ├── rtrt-proxy/src/lib.rs
│   ├── rtrt-memory/src/lib.rs
│   ├── rtrt-providers/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── anthropic.rs
│   │       ├── openai.rs
│   │       └── openai_compatible.rs
│   ├── rtrt-templates/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── builtin.rs
│   │       ├── custom.rs
│   │       └── render.rs
│   ├── rtrt-mcp/src/main.rs
│   ├── rtrt-dashboard/src/main.rs
│   └── rtrt-cli/src/main.rs
└── docs/                            # bilingual documentation
```

## Data flows

### Compression

1. Caller (`rtrt compress`, `rtrt-mcp`, or library user) constructs `Compressor::new(level)`.
2. `compress(&input)` runs the **stash → rules → restore** pipeline.
3. Returns the rewritten `String`.

The compressor is `Copy` and holds no per-call state.

### Memory

1. `MemoryStore::open(path)` runs the migration if needed.
2. `save(project, kind, body)` inserts a row into `memories` and a mirror into `memories_fts`.
3. `recall_bm25(project, query, limit)` joins `memories_fts` (ranked) against `memories` filtered by `project`.

Vector recall + graph traversal are reserved for v0.2 — the tables exist but no code path writes to them yet.

### Templates

1. `list_all()` returns built-in templates (`builtin::ALL`) plus any custom templates found via `custom::scan_default_dir()`.
2. `find(name)` resolves a single template.
3. `render::plan(template, target_dir, vars)` validates required variables, applies defaults, and produces a `RenderPlan` with absolute file paths and substituted content.
4. `render::write(plan, overwrite)` writes the files and sets the executable bit where requested.
5. Post-init hooks run via `std::process::Command` with the hook line split on whitespace (no shell).

### Provider chat (planned)

The trait is wired but the chat implementations return `Error::Provider(...)`. v0.2 will route requests through `reqwest` with provider-specific headers and parse the streaming response.

## Concurrency model

- Async runtime: `tokio` multi-threaded (`rt-multi-thread`).
- Memory store is synchronous (`rusqlite` is blocking); call sites that need async access wrap calls in `tokio::task::spawn_blocking`.
- HTTP server uses `axum` 0.8 on top of `hyper`.
- HTTP client uses `reqwest` 0.12 with `rustls-tls` (no platform OpenSSL dependency).

## Build profiles

- `dev` — `opt-level = 0`, `debug = true`. Default for `cargo build`.
- `release` — `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`. Used for distributed binaries.
