# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Highlights

**rtrt-providers, rtrt-mcp, and rtrt-memory go from stubs to real implementations; install + release + license-gate automation lands.**

- `rtrt-providers` chat + streaming against Anthropic and OpenAI; OpenAI-compatible adapter covers Ollama / llama.cpp / vLLM / LM Studio. Usage is parsed for both providers and flows into the dashboard. (inspired by [Helicone](https://github.com/Helicone/helicone), [llm-chain](https://github.com/sobelio/llm-chain))
- `rtrt-mcp` ships a real stdio MCP server via [`rmcp`](https://crates.io/crates/rmcp) 1.x exposing `compress`, `memory_save`, `memory_recall`, `templates_list`, `templates_scaffold`.
- `rtrt-memory` adds local `all-MiniLM-L6-v2` embeddings (`fastembed`, behind the `embeddings` feature) plus BM25 + vector hybrid recall via Reciprocal Rank Fusion. (inspired by [mem0](https://github.com/mem0ai/mem0), [chroma](https://github.com/chroma-core/chroma), [qdrant](https://github.com/qdrant/qdrant))
- `rtrt-memory` ships the `Summariser` trait + `LlmSummariser` (behind the `llm` feature) so memory extraction and compression work with any provider — including a local Ollama server through the existing OpenAI-compatible adapter, no new HTTP code. `rtrt memory extract` and `rtrt memory compress` CLI commands expose the flow. (inspired by [mem0](https://github.com/mem0ai/mem0) ADD-only extraction, [MemGPT](https://github.com/cpacker/MemGPT) virtual-context paging)
- `rtrt-compress` `criterion` benchmark harness — the README's compression-savings claim is now testable.
- `install.sh` + `install.ps1` one-liners wired to GitHub Releases with SHA256 verification; `release.yml` builds 5 targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`), attaches them to the GitHub Release, and publishes all 9 crates to crates.io on a `REL-vX.Y.Z` marker tag.
- `cargo-deny` license + advisory + bans + sources gate, blocking on PRs to `main` and on a weekly cron.
- New [`docs/INSPIRATION.md`](docs/INSPIRATION.md) — 50+ borrow ideas from 18 reference projects mapped to specific RTRT crates with priority.

### Added

- `rtrt-providers`: real `chat()` + `chat_stream()` implementations against Anthropic Messages API and OpenAI Chat Completions API; `OpenAICompatibleProvider` wraps the OpenAI adapter with a user-supplied base URL; shared SSE decoder; `Usage { input_tokens, output_tokens, cache_read, cache_creation }` with `merge` / `total`; `ChatStreamEvent::{ Delta, Usage, Done }`. mockito unit tests round-trip unary chat for both providers.
- `rtrt-cli`: `rtrt provider chat --model … [--stream] [--system …]` and `rtrt memory {save,recall,extract,compress}` subcommands.
- `rtrt-memory`: `Embedder` trait, `FastEmbedder` (`embeddings` feature, `all-MiniLM-L6-v2`), `save_embedded`, `recall_vector`, `recall_hybrid` (Reciprocal Rank Fusion, `rrf_k = 60`), `list_by_project`, `delete`, `Summariser` trait, `LlmSummariser` (`llm` feature), `extract_and_save`, `compress_project`.
- `rtrt-mcp`: 5 tools over rmcp stdio; `--memory` flag selects the SQLite store; logs to stderr.
- `rtrt-compress`: criterion benches across `lite` / `full` / `ultra` × 4 fixtures (short, code, mixed, long); `scripts/bench.sh` prints the savings table.
- `install.sh` + `install.ps1`: detect OS+arch, resolve latest release, download tarball/zip, SHA256-verify, drop binaries to `~/.local/bin` (Linux/macOS) or `%LOCALAPPDATA%\Programs\rtrt\` (Windows). `--main` fallback builds from source, `--uninstall` removes the three binaries.
- `.github/workflows/release.yml`: tagged-release builds 5-target matrix, extracts the CHANGELOG section on `REL-` tags, publishes crates.io in dependency order.
- `.github/workflows/deny.yml`: blocking `cargo deny check licenses sources bans advisories` on every push/PR/weekly cron.
- `deny.toml`: license allowlist (MIT, Apache-2.0, BSD-{2,3}-Clause, ISC, MPL-2.0, Unicode-3.0, Zlib, BSL-1.0, OpenSSL exception for `ring`); copyleft denied.

### Changed

- `rtrt-core`: `CompressionLevel` and `Config` switch to `#[derive(Default)]` with `#[default]` enum variant; manual impls removed (clippy `derivable_impls`).
- `rtrt-providers` workspace deps add `eventsource-stream`, `futures-util`, `mockito`.
- `Cargo.toml` adds workspace deps for `rmcp`, `schemars`, `criterion`, `fastembed`, `eventsource-stream`, `futures-util`, `mockito`.

### Fixed

- AIPS plugin workaround at init time: `lib/detect-project.sh` emits unquoted multi-word values (e.g. `DEPLOYMENT=GitHub Actions`), which breaks `lib/render-claude-md.sh`'s `eval` call. Worked around locally; upstream bug body saved to `.priv-storage/sessions/aips-upstream-issue-body.md`.

<!--
Template for each new version section — copy this stanza when cutting a release.
Keep `### Highlights` at the very top: it is the first thing users see on the
GitHub release page because `release.yml`'s extract takes the section verbatim.
-->

## [0.1.0] - 2026-05-20

### Highlights

**Initial workspace scaffold. Output compression, command-output filtering, SQLite-FTS5 BM25 recall, and project-template scaffolding all run end-to-end; MCP transport, provider chat clients, and install scripts are explicit stubs.**

- Cargo workspace with 9 crates on edition 2024 (`rtrt-core`, `rtrt-compress`, `rtrt-proxy`, `rtrt-memory`, `rtrt-providers`, `rtrt-templates`, `rtrt-mcp`, `rtrt-dashboard`, `rtrt-cli`).
- `rtrt-compress` ships a caveman-style rewriter with `lite` / `full` / `ultra` levels; code blocks, inline code, URLs, and quoted error strings are stashed before the rule pass and restored afterwards.
- `rtrt-proxy` ships filters for `git status`, `git log`, `cargo build`, `cargo test`; the CLI exposes `rtrt proxy "<cmd>"` for stdin → filtered stdout.
- `rtrt-memory` ships a SQLite + FTS5 schema with `memories / memories_fts / embeddings / edges` tables and BM25 recall via the `recall_bm25` API.
- `rtrt-templates` ships six built-ins (`rust-cli`, `rust-lib`, `rust-axum`, `node-typescript`, `python-uv`, `go-cli`) and a custom loader from `~/.rtrt/templates/<name>/manifest.toml`. End-to-end smoke: `rtrt new rust-cli` produces a project whose `cargo check` passes.
- `rtrt-dashboard` ships an axum server with `/`, `/healthz`, `/api/stats`, `/api/templates`, `/api/templates/{name}`, and `/api/templates/scaffold`.

### Added

- Workspace scaffold, MIT LICENSE, GitHub repo standardisation (issue / PR templates, FUNDING.yml, CI workflow), bilingual docs/ tree (`INSTALL`, `USAGE`, `FEATURES`, `ARCHITECTURE`, `COMPARISON`, `README.ko`, plus `*.ko` mirrors).
- `Compressor::compress` with rule-protection for code blocks, inline code, URLs, and `"quoted strings"`.
- `rtrt_proxy::filter_for` dispatch table; `git_status`, `git_log`, `cargo_noise` filters; `collapse_blanks` helper.
- `MemoryStore::open`, `MemoryStore::open_in_memory`, `MemoryStore::save`, `MemoryStore::recall_bm25`.
- `Provider` trait + Anthropic / OpenAI / OpenAI-compatible adapter stubs.
- `rtrt-templates` `Template`, `TemplateFile`, `TemplateVariable`, `RenderPlan`; built-in template programmatic definitions; custom `manifest.toml` loader; `{{var}}` substitution; optional post-init shell hooks.
- `rtrt` CLI subcommands: `compress`, `proxy`, `templates`, `new`, `info`.
- Axum dashboard with template gallery + scaffold endpoint.

### Notes

- MCP stdio transport is not implemented; `rtrt-mcp` logs the planned tools and exits.
- Provider `chat` returns `Error::Provider("... not implemented yet")`; only model lists and adapter shapes are wired.
- `rtrt-memory` has no embeddings yet; the `embeddings` and `edges` tables are reserved.
- `install.sh` / `install.ps1` are referenced in the README but not yet present in the tree.
