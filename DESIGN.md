# RTRT — Design Principles

**English** | [한국어](docs/DESIGN.ko.md)

RTRT exists to **reduce the tokens AI agents waste while preserving meaning**, and to **remember everything an agent does** so the next session starts where the last one stopped. Every other capability — compression, command-output filtering, persistent memory, multi-provider routing, project scaffolds — serves those two goals.

The repo is intentionally narrow. The discipline this file describes is what keeps it that way.

## 1. Unix-philosophy toolkit, not a framework

Frameworks die. Tools live.

- Atom editor, Meteor.js, CoffeeScript, Backbone — frameworks that owned a moment and disappeared in five years.
- `grep`, `awk`, `sed`, SQLite, `ripgrep`, `fd`, `jq` — single-purpose tools that have outlived every framework around them.

RTRT ships single-purpose binaries:

```bash
rtrt compress -l ultra < verbose.md
rtrt memory recall --project rtrt --query auth
rtrt signatures --lang rust < src/lib.rs
rtrt-mcp --transport stdio
rtrt-dashboard
```

Each tool reads stdin, writes stdout, exits. Each is composable with `|`, `>`, `&`. None depends on a daemon. None requires a framework. Every CLI flag added is one we commit to keeping for years.

## 2. Lean on stable substrates

Long-lived software is built on substrates older than itself.

| Substrate | Why it lasts | Where RTRT uses it |
|----------|--------------|--------------------|
| SQLite | Public domain, ~30 years old, embedded | `rtrt-memory` storage |
| FTS5 / BM25 | SQLite-native full-text search | `recall_bm25` |
| Markdown | Plain text, version-controllable | templates + prompts |
| JSON Lines | One record per line, grep-friendly | `memory export/import` |
| SHA-256 | Cryptographic primitive, decades stable | dedup index |
| Rust (edition 2024) | Memory-safe, stable ABI for tooling | every crate |
| tree-sitter | Editor-grade parser, used by GitHub / Neovim / Helix | `signatures` / `repo-map` |
| MCP (Model Context Protocol) | Open standard, multi-vendor | `rtrt-mcp` |
| Server-Sent Events | RFC 6202, widely supported | `/api/stream` |
| Unix pipes / stdio | Older than this project's authors | every CLI |

We do **not** bet the project on:

- Any single LLM vendor's API surface (absorbed behind the `Provider` trait).
- Any single agent framework (LangChain, AutoGen, CrewAI — these move weekly).
- Cloud-only services (AWS, GCP, OpenAI as the only provider, etc.).
- Hype-cycle abstractions (skill systems, virtual agents, multi-agent meshes that have less than 2 years of production track record).

## 3. Three pillars, nothing more

Every feature must fit one of these:

1. **Token reduction.** Make the prompt smaller without losing meaning.
   - `rtrt-compress` rule + ML engines, tree-sitter signature extractor, secrets redactor, chroma-style multi-format output.
2. **Persistent project memory.** Capture what an agent did. Recall it later. Survive across sessions, machines, and tools.
   - `rtrt-memory` SQLite + FTS5 + vector + RRF hybrid + graph + dedup + privacy filter + hourly consolidation.
3. **Multi-provider routing with local-first defaults.** Any LLM behind one trait. Ollama / OpenAI / Anthropic interchangeable. Budget-aware. Response-cached.
   - `rtrt-providers` Gateway + Budget + retry / fallback + Helicone-style response cache.

If a proposed feature does not fit one of these three buckets, it goes into an **optional crate** (see §5) or gets rejected.

## 4. Auto-capture by default

The memory store is useless if writing to it is manual. Auto-capture is a first-class behaviour, not an opt-in.

The pipeline that runs on every captured event:

```
event fires
  ├─ 1. SHA-256 dedup (5-minute window, configurable)
  ├─ 2. Privacy filter (AWS / GitHub / OpenAI / Anthropic / Slack / Bearer
  │     / private-key / api_key=… redacted before storage)
  ├─ 3. Raw observation saved to SQLite (FTS5 + BM25 auto-indexed)
  ├─ 4. Session id tag (one UUID per process; hooks pass their own)
  └─ 5. Optional LLM compression to facts / concepts in a background task
```

Environment variables expose every knob (see [`docs/USAGE.md`](docs/USAGE.md)):

| Env | Default | Effect |
|------|---------|--------|
| `RTRT_AUTO_CAPTURE` | `1` | Master switch |
| `RTRT_AUTO_REDACT` | `1` | Privacy filter on/off |
| `RTRT_AUTO_DEDUP_WINDOW_SEC` | `300` | Dedup window |
| `RTRT_CONSOLIDATE_INTERVAL_SEC` | `3600` | Hourly archive sweep |
| `RTRT_CONSOLIDATE_KEEP` | `1000` | Rows kept per project after sweep |

Every record is **permanent until the user explicitly removes it**. Consolidation summarises and prunes; it never silently drops a row that has not exceeded the keep threshold.

## 5. Optional crates instead of scope creep

When a feature is tempting but does not fit the three pillars, it becomes an optional crate that ships separately or behind a feature flag.

Examples already in the tree:

- `rtrt-compress[treesitter]` — tree-sitter grammars (off by default; adds 30MB to the artefact).
- `rtrt-compress[llm-compress]` — LLM-backed compression path.
- `rtrt-memory[embeddings]` — fastembed ONNX runtime.
- `rtrt-memory[hnsw]` — `instant-distance` ANN index.
- `rtrt-memory[llm]` — `LlmSummariser` wrapper.

Future candidates kept out of the core:

- `rtrt-orchestrator` — multi-agent coordination (actions / signals / leases / mesh / sentinels). Implementing this in the core would couple our SQLite schema to ideas that are still evolving in the wider ecosystem. It belongs in its own crate, behind its own opt-in.
- `rtrt-snapshot` — git-versioned memory snapshots.
- `rtrt-eval` — recall accuracy benchmarks against labelled datasets.

The default install stays light.

## 6. Performance is measured, not claimed

We do not say "blazingly fast." We say "p99 of 443µs on 100k rows, measured on commit a1b2c3 on a 2024 laptop." See [`docs/PERF.md`](docs/PERF.md) for the SLO table.

Every release re-runs the criterion suite. Regression beyond 10% is a release blocker.

## 7. Local-first, privacy-first

- The default install talks to **no external service**. SQLite is local. The dashboard binds to loopback. The compress + proxy + repo-map paths run entirely offline.
- LLM calls go to whatever endpoint the user configures — Ollama on `127.0.0.1:11434`, a self-hosted vLLM, or a vendor's cloud. The user picks.
- Secrets are redacted from the auto-capture path before they hit disk.
- The bearer-token middleware on `rtrt-mcp` HTTP / `rtrt-dashboard` blocks unauthenticated access outside loopback.

## 8. Interfaces are forever once published

Once a CLI flag, an MCP tool name, a SQLite schema column, or a JSON field ships in a tagged release, it does not change. Additions only. The data formats are forward-portable: every memory store from v0.1.0 onward must open in every future version.

Schema migrations bump `PRAGMA user_version` and only **add** columns / indexes. Renames and removals require a major version bump and a written migration plan in `CHANGELOG.md`.

## 9. Small, slow, deep

- One quarter ships one or two real features, not twelve.
- Polish takes priority over breadth. A bug-free `memory_save` is worth more than five half-built tools.
- Documentation lands with the code, not after.
- Korean and English docs stay in sync.

## 10. Acceptable risks

We accept that:

- The MCP standard is young. We track it but do not bet the project on it. CLI + library surfaces stay regardless.
- Vector embeddings depend on third-party models. We default to a local model and treat cloud embeddings as opt-in.
- The auto-capture path will collect some noise. Dedup + privacy filter mitigate; consolidation prunes. We prefer "too much, summarisable" over "missed an event."

---

## What this rules out

- **53-tool MCP surfaces.** We will not race to feature parity with broader memory platforms. We will ship 10-15 well-thought MCP tools and stop.
- **Multi-agent coordination in the core.** Signals, leases, mesh sync, sentinels are out. If they prove durable in the next 18 months, `rtrt-orchestrator` picks them up.
- **Cloud-only or paid-only features.** Everything in this repo runs offline on a laptop.
- **Frameworks built on top of frameworks.** No agent runtime, no orchestration DSL, no plugin marketplace.

This file is intentionally short. Re-read it before adding a new top-level feature.
