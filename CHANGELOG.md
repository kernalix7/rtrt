# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] - 2026-09-14

### Highlights

**RTRT 0.1.2 is a hardening release: the OpenCode statusline stops starving keyboard input, proxy stats stay private on Unix, and the release contract pins `rtrt-agent@0.1.2` under trusted publishing.**

- The OpenCode statusline samples session economics only on scoped lifecycle/status events, so long-running local sessions no longer lose keyboard input to statusline render loops.
- Proxy stats storage on Unix is created private (`~/.rtrt` at `0700`, `proxy-stats.sqlite` and sidecars at `0600`), with legacy modes repaired and unsafe paths rejected.
- OpenCode setup registers the exact `rtrt-agent@0.1.2` package, and the paired-tag release contract plus its preflight checks are hardened around npm trusted publishing.

### Changed

- OpenCode setup now registers the exact `rtrt-agent@0.1.2` npm package through the singular root `plugin` key; existing `0.1.1` registrations are rewritten on the next `rtrt setup --agent opencode --apply`.
- The paired-tag release contract is hardened: the `vX.Y.Z` and `REL-vX.Y.Z` runs verify that the tag, workspace version, `rtrt-agent` package version, plugin metadata, and changelog section all agree before building, and the npm trusted-publishing step refuses to run when that preflight fails.

### Fixed

- The OpenCode statusline no longer subscribes to high-volume file and message-part events, tracks streaming message state from Solid render computations, broadcasts unscoped events to every mounted session, or renders inside the prompt-right input path. Session economics are sampled only on scoped lifecycle/status events in the application-bottom surface, preventing statusline render loops from starving keyboard input on long-running local sessions.
- Proxy stats storage is now private on Unix: the default `~/.rtrt` directory is created at `0700`, and `proxy-stats.sqlite` plus any existing `-wal`, `-shm`, or `-journal` sidecars are held at `0600`. Owner-owned files left with looser legacy modes are repaired on the next writable stats access, while symlinks, non-regular files, and paths owned by another user are rejected. When `RTRT_PROXY_STATS_PATH` points elsewhere, the permissions of that parent directory are left untouched.

## [0.1.1] - 2026-09-10

### Highlights

**RTRT 0.1.1 finalizes the local-first toolkit release; multi-agent orchestration remains the host runtime's responsibility.**

- 11-crate workspace, three product binaries, opt-in `rtrt-eval`, 23 MCP tools, and four built-in templates: `dev`, `design`, `plan`, and `standardization`.
- OpenCode integration registers the exact `rtrt-agent@0.1.1` npm package and remains compatible with OMO. `rtrt-agent` is published to npm via trusted publishing; workspace crates stay source-only.
- Paired-tag release automation: the `vX.Y.Z` tag run validates and builds the Rust binaries, publishing them only as Actions artifacts; the `REL-vX.Y.Z` tag run rebuilds, publishes `rtrt-agent` to npm via trusted publishing, then creates/updates the GitHub Release under `vX.Y.Z` and attaches the five per-platform binary archives plus their checksums. GitHub auto-generates the source archive from the `vX.Y.Z` tag.
- Setup hardening adds strict preflight, symlink protection, and atomic writes for managed OpenCode state.

### Added

- OpenCode npm plugin integration through the singular root `plugin` key with the exact `rtrt-agent@0.1.1` registration; the setup-managed statusline remains outside the npm package.
- Project-private OpenCode session migration and launcher support, plus safer setup-owned Linux shell sandboxing.

### Changed

- Provider routing and failover remain toolkit features; native orchestration, teams, schedulers, rosters, and `team_dispatch` are removed and delegated to the host runtime.
- The dashboard Orchestration page becomes Failover, served at `/failover` and backed by `/api/failover/config`; `/orchestration` resolves to the overview and `assets/js/orchestration.js` no longer exists.
- Failover policy is editable without a project selected: no selector reads and writes the global `[failover]` policy, a selected project inherits it read-only, **Custom** writes `<repo>/.rtrt/config.toml`, and **Follow global** removes only that override.
- OpenCode setup validates strict legacy cleanup before any Rules, TUI, or MCP writes, then performs recognized cleanup last; any earlier failure preserves the legacy runtime.
- The unified release workflow provides paired-tag release automation from one versioned release. The `vX.Y.Z` tag run triggers the `release.yml` validate-and-build job, which produces the per-platform Rust binaries and publishes them only as Actions artifacts. The `REL-vX.Y.Z` tag run re-runs the build, runs the npm publish job (trusted publishing pushes `rtrt-agent` to npm), then creates/updates the GitHub Release under `vX.Y.Z` and attaches the five per-platform binary archives plus their checksums. GitHub auto-generates the source archive from the `vX.Y.Z` tag. Workspace crates are not published to crates.io.

### Fixed

- Hardened setup, provenance, permission, filesystem, and HTTP MCP boundaries; updated `h2` to 0.4.16 for security fixes.
- Retiring a legacy project file now backs it up through an exclusive create, verifies the target is a regular file whose bytes and ownership still match, and re-verifies immediately before it mutates that specific file. The replacement itself lands in a sibling temporary file and is renamed into place, so a symlink swapped in at the last moment is replaced rather than written through.
- `<repo>/.rtrt` is created with `0700` in a single step instead of being created and then narrowed, closing the window in which the later permission change could be redirected.
- `rtrt-mcp --transport http` with no `--allowed-origins` now rejects every request that carries an `Origin` header instead of accepting any origin. Native clients, which send no `Origin`, are unaffected.
- The OpenCode plugin no longer auto-approves shell commands that name a path. Permission evaluation and shell execution resolve paths at different times, so an approved file could be swapped for a symlink in between; `cat`, `ls`, `head`, `tail`, `wc`, and `stat` now fall back to a normal prompt and only `pwd` is auto-approved.
- `rtrt-core` builds on Windows again. Its same-file check used the still-unstable `volume_serial_number`/`file_index` pair, so the crate only compiled on nightly. Windows now compares every stable attribute instead, which detects the swap this guards against but is a tamper check rather than true file identity.
- The dashboard failover editor rejects `transient_retries` and `backoff_divisor` above `u32::MAX` with a 400 instead of silently truncating them, and a failed policy load now clears and locks the form so stale project values can never be saved into the global policy.
- Improved setup idempotence, project-private session migration and catch-up, sandbox validation, uninstall ordering, and preservation of foreign configuration.

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
