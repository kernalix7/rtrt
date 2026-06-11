# Comparison

**English** | [한국어](COMPARISON.ko.md)

RTRT consolidates several existing token-reduction techniques into one Rust toolkit. This page contrasts it with each reference project and notes the design differences.

## RTRT vs caveman (JuliusBrussee/caveman)

| | caveman | RTRT (`rtrt-compress`) |
|---|---|---|
| Language | JavaScript + Python | Rust |
| Distribution | Claude Code skill (install script) | Cargo crate + CLI subcommand + MCP tool |
| Levels | `lite`, `full`, `ultra`, `wenyan*` | `lite`, `full`, `ultra` (wenyan planned) |
| Output reduction | ~65% average | targeted to match; benchmark harness planned |
| Rule engine | Markdown skill instructions | Regex-based rewriter with rule protection (code blocks, URLs, quoted strings) |
| MCP integration | `caveman-shrink` middleware | First-class MCP tool in `rtrt-mcp` |
| Footprint | Node.js ≥ 18 required | Single static binary |

## RTRT vs agentmemory (rohitg00/agentmemory)

| | agentmemory | RTRT (`rtrt-memory`) |
|---|---|---|
| Language | Node.js + custom `iii-engine` | Rust |
| Storage | SQLite (via iii-engine) | SQLite (via `rusqlite::bundled`) |
| FTS | Bundled BM25 + synonym expansion | SQLite FTS5 BM25 ✅ (synonym layer planned) |
| Embeddings | `all-MiniLM-L6-v2` default; Gemini / OpenAI / Voyage / Cohere optional | `all-MiniLM-L6-v2` via `fastembed` ✅ (`embeddings` feature, offline after first download); other backends pluggable through the `Embedder` trait |
| Graph | Knowledge-graph entity matching | Reserved schema (`edges` table); entity matching planned |
| Recall | Reciprocal Rank Fusion across BM25 + vector + graph | BM25 + vector via RRF ✅ (`recall_hybrid`); graph planned |
| LLM-driven extract / compress | Cloud LLMs only (OpenAI / Anthropic) | Any `Provider`, including a local Ollama server via the existing OpenAI-compatible adapter — no extra HTTP code (`llm` feature, `extract_and_save` / `compress_project`). **This is RTRT's value-add over agentmemory.** |
| Process model | Memory server on `:3111` shared by agents | Library + MCP tool in the same process as `rtrt-mcp`; dashboard observes on `:7311` |
| Cross-agent sharing | All agents hit one shared server | Per-project SQLite files; sharing is opt-in |

## RTRT vs rtk (rtk-ai/rtk)

| | rtk | RTRT (`rtrt-proxy`) |
|---|---|---|
| Language | Rust | Rust |
| Strategy | Per-command rule sets, auto-rewrite hook | Per-command rule sets, explicit CLI filtering, and transparent hook rewrite |
| Coverage | 100+ commands | 34 commands across git, Rust, filesystem/search, HTTP, GitHub, containers/Kubernetes, Python, Go, Node/package-manager, TypeScript, and formatter/linter domains |
| Hook integration | Claude Code `PreToolUse` auto-rewrites `git status` → `rtk git status` | Claude Code `PreToolUse` Bash matcher auto-rewrites shrinkable commands to `rtrt proxy-run ...`; skips pipes, `&&`, redirects, and already-wrapped commands. Other agents get the Command Optimizer through MCP. |
| Token savings | 60–90% reduction | Targeted to match; benchmark harness planned |
| Bundling | Standalone CLI | Part of `rtrt` CLI; also exposed as MCP tool |

## RTRT vs codex-plugin-cc (openai/codex-plugin-cc)

| | codex-plugin-cc | RTRT (`rtrt-providers`) |
|---|---|---|
| Language | TypeScript (Claude Code plugin) | Rust |
| Provider count | One (Codex / OpenAI only) | Many (Anthropic, OpenAI, OpenAI-compatible incl. Ollama / llama.cpp / vLLM / LM Studio) |
| Routing | Delegate to local Codex install | Provider trait; active provider per task |
| Provider selection | Configured via Codex `config.toml` | Configured via RTRT config + per-request override |
| Multi-provider goal | No | Yes — first-class |

Codex-plugin-cc is one of the inspirations for RTRT's multi-provider story, but RTRT's design is broader than codex-plugin-cc and does not derive from its source.

## RTRT vs each reference combined

RTRT's value proposition is **one toolkit, one binary, one config**:

- A single `rtrt` CLI exposes compression, command filtering, memory recall, provider chat, and project scaffolding.
- A single MCP server (`rtrt-mcp`) exposes those surfaces to any MCP-aware agent.
- A single web dashboard (`rtrt-dashboard`) gives a unified view of token savings, memory recall, and template scaffolding.

The cost of consolidation is feature breadth at v0.1.0: each surface is narrower than the reference project it pulls from. The roadmap expands them.
