Sure, I'd be really happy to walk through the entire architecture in detail. Let me actually start with the high-level overview and then drill into each crate one by one.

The RTRT toolkit is basically a Rust workspace with nine separate crates. The reason we split it up this way is that each crate is responsible for a single token-reduction surface, and we really want each one to be testable in isolation. Let me just enumerate them for you in dependency order.

First, there's `rtrt-core`. This is the shared crate that everyone else depends on. It defines the basic types like `Error`, `Result`, `CompressionLevel`, `TokenCount`, `TokenStats`, the `Plugin` trait, and the `Config` struct. The reason we keep this one really lean is so that the dependency graph stays shallow. If `rtrt-core` ever needed to grow a heavy dependency, we'd want to push that down into a more specific crate instead.

Next, there's `rtrt-compress`. This is the output-compression engine, basically a regex-based rewriter that's inspired by the caveman project but written from scratch in Rust. The really clever part of the implementation is the protection phase: before any rule runs, we scan the input for code blocks, inline code, URLs, and quoted strings, and we replace each one with an opaque placeholder. Then the rule phase applies its substitutions, and the restore phase swaps the placeholders back. This ensures that technical content is never accidentally rewritten.

After that comes `rtrt-proxy`. This is the command-output filter, modeled after the rtk project. The basic idea is that lots of CLI tools produce really noisy output by default — `git status` is the classic example, with all those `(use ...)` hint lines and `On branch main` headers. The proxy applies per-command regex rules to strip the noise. We currently ship filters for `git status`, `git log`, `cargo build`, and `cargo test`, but the dispatch table is open to extension.

Then there's `rtrt-memory`. This is the persistent-memory layer, basically a SQLite database with an FTS5 virtual table for BM25 recall. The schema has four tables: `memories` (the source of truth), `memories_fts` (the FTS5 index), `embeddings` (vector data), and `edges` (graph relationships). In v0.1.0 we only implement BM25; the embeddings and edges tables are reserved for v0.2 where we'll add `all-MiniLM-L6-v2` embeddings and Reciprocal Rank Fusion.

`rtrt-providers` is the multi-provider abstraction. The `Provider` trait has methods `name()`, `supported_models()`, and `chat()`. The built-in adapters cover Anthropic, OpenAI, and a generic OpenAI-compatible endpoint that you can point at Ollama, llama.cpp, vLLM, or LM Studio. In v0.1.0 the `chat()` implementations all return an error because we haven't wired the HTTP layer yet; that's a v0.2 deliverable.

`rtrt-templates` is the project-scaffolding crate. It ships six built-in templates: `rust-cli`, `rust-lib`, `rust-axum`, `node-typescript`, `python-uv`, and `go-cli`. It also has a custom-template loader that picks up TOML manifests from `~/.rtrt/templates/<name>/`. The variable substitution uses `{{key}}` syntax, and paths can contain substitutions too. Post-init hooks are run via `std::process::Command` with the hook line split on whitespace, no shell involved.

`rtrt-mcp` is the MCP server binary. In v0.1.0 it's just a stub that announces the planned tool surface and exits. In v0.2 we'll wire it up to the `rmcp` crate, which is the official Rust MCP SDK, and expose `compress`, `memory.save`, `memory.recall`, `provider.chat`, `templates.list`, and `templates.scaffold` over both stdio and HTTP/SSE.

`rtrt-dashboard` is the axum-based web UI. It serves a minimal HTML index plus a REST API with endpoints for token-savings statistics, template listing, individual template manifests, and scaffold execution. The dashboard binds to `127.0.0.1:3111` by default, configurable via `RTRT_DASHBOARD_BIND`. Remote exposure is opt-in by design.

Finally, `rtrt-cli` is the top-level binary. It's just a thin clap-based wrapper that dispatches to the underlying crates. The subcommands are `compress`, `proxy`, `templates`, `new`, and `info`.

The whole thing is built on edition 2024 with resolver v3, pinned to stable Rust 1.85+, and uses tokio multi-threaded runtime, axum 0.8, reqwest 0.12 with rustls-tls, and rusqlite with the bundled SQLite feature. There's literally no `unsafe` block in any of the core crates, and clippy is gated at `-D warnings` in CI.
