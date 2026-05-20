# RTRT — Rust-based Token Reduction Toolkit

> Cut input and output tokens for AI agents. One install. Rust-stable core.

RTRT is a Rust toolkit that combines four token-reduction techniques behind one CLI, one MCP server, and one web dashboard:

| Technique | Reference | Crate |
| --- | --- | --- |
| Output simplification (caveman-style terse mode) | [`JuliusBrussee/caveman`](https://github.com/JuliusBrussee/caveman) | `rtrt-compress` |
| Persistent project memory (SQLite + hybrid recall) | [`rohitg00/agentmemory`](https://github.com/rohitg00/agentmemory) | `rtrt-memory` |
| Command-output filtering proxy | [`rtk-ai/rtk`](https://github.com/rtk-ai/rtk) | `rtrt-proxy` |
| Multi-provider AI routing | inspired by [`openai/codex-plugin-cc`](https://github.com/openai/codex-plugin-cc) | `rtrt-providers` |
| Standardized project scaffolds (built-in + custom, web-selectable) | new | `rtrt-templates` |

## Status

Early scaffold — v0.1.0. Crates compile, public APIs are stubs. See [Roadmap](#roadmap).

## Install

> Planned. Not yet wired.

```bash
# macOS / Linux / WSL
curl -fsSL https://raw.githubusercontent.com/kernalix7/rtrt/main/install.sh | sh
```

```powershell
# Windows
irm https://raw.githubusercontent.com/kernalix7/rtrt/main/install.ps1 | iex
```

From source:

```bash
git clone https://github.com/kernalix7/rtrt
cd rtrt
cargo install --path crates/rtrt-cli
```

## Architecture

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
| rtrt-  |   | rtrt-   |    | rtrt-    |    | rtrt-       |    | plugins |
| compr. |   | proxy   |    | memory   |    | providers   |    | (later) |
+--------+   +---------+    +----------+    +-------------+    +---------+
```

## Crates

- **`rtrt-core`** — shared types (`Token`, `Compression`, `Provider`), plugin trait, config loader.
- **`rtrt-compress`** — output compression engine; caveman-style rules at levels `lite`, `full`, `ultra`.
- **`rtrt-proxy`** — command output filter; `rtrt git status` → compact form.
- **`rtrt-memory`** — SQLite-backed memory with hybrid recall (BM25 + vector + graph), local-first embeddings.
- **`rtrt-providers`** — provider abstraction (Anthropic, OpenAI, Google, xAI, Mistral, local OpenAI-compatible). Active provider selection.
- **`rtrt-templates`** — standardized project scaffolds. Built-in: `rust-cli`, `rust-lib`, `rust-axum`, `node-typescript`, `python-uv`, `go-cli`. Custom templates load from `~/.rtrt/templates/<name>/manifest.toml`. Web-selectable via dashboard.
- **`rtrt-mcp`** — MCP server exposing compress / memory / providers as tools.
- **`rtrt-dashboard`** — axum web UI: token savings, recall stats, provider routing.
- **`rtrt-cli`** — top-level binary: install / init / proxy / serve / plugin.

## Features (target)

- **One-line install** with managed updates.
- **Plugin format** — load external compression rules and provider adapters at runtime.
- **MCP server** — stdio and HTTP transports.
- **Web dashboard** — per-project token-savings analytics.
- **Multi-provider** — switch providers per task without leaving the agent.
- **Standardized project layouts** — pick a template in the web dashboard or CLI (`rtrt new <template> <path>`); custom templates are first-class.
- **Rust stability** — zero-panic core, fuzz-tested filters.

## Roadmap

- [x] Workspace scaffold
- [x] `rtrt-core` plugin trait + config schema
- [x] `rtrt-compress` rule engine (lite/full/ultra, code-block-safe)
- [x] `rtrt-proxy` git/cargo command rules
- [x] `rtrt-memory` SQLite schema + BM25 recall
- [x] `rtrt-templates` 6 built-in scaffolds + custom loader
- [x] `rtrt-dashboard` minimal axum UI with `/api/templates`
- [ ] `rtrt-compress` benchmark harness
- [ ] `rtrt-memory` `all-MiniLM-L6-v2` embeddings + vector index
- [ ] `rtrt-providers` real Anthropic + OpenAI clients (chat is currently a stub)
- [ ] `rtrt-mcp` real stdio transport (currently exits after announcing tools)
- [ ] One-line install scripts (`install.sh` / `install.ps1`)
- [ ] Claude Code plugin manifest

## License

[MIT](LICENSE) © 2026 Kim DaeHyun (kernalix7@kodenet.io)
