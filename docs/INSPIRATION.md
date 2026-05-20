# Inspiration

**English** | [한국어](INSPIRATION.ko.md)

RTRT borrows ideas from many other token-reduction, memory, and agent-tooling projects. This page lists each source, the specific idea worth borrowing, the RTRT crate it would land in, and a priority guess. Adoption appears in `CHANGELOG.md` with an inline `(inspired by [...])` credit; legal attribution lives in [`THIRD_PARTY_LICENSES.md`](../THIRD_PARTY_LICENSES.md#reference-projects-inspiration-only-no-code-redistributed).

Priority key — **high** = clear win, queue for next minor; **medium** = future, deferred until the surface stabilises; **low** = stretch / nice-to-have.

## Output compression

| Project | Idea | Fits | Priority |
|---------|------|------|----------|
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | MCP middleware wrapping tool descriptions (`caveman-shrink`) | `rtrt-mcp` | high |
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | `/caveman-compress` rewrites memory files permanently | `rtrt-compress` + `rtrt-memory` | high |
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | Statusline badge with cumulative tokens saved | `rtrt-dashboard` + `rtrt-proxy` | medium |
| [JuliusBrussee/caveman](https://github.com/JuliusBrussee/caveman) | Wenyan (classical-Chinese) variant for extra compression | `rtrt-compress` | low |
| [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua) | Small-LM token classifier prunes non-essential tokens (~20× reduction) | `rtrt-compress` | high |
| [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua) | LongLLMLingua: reorder context + dynamic ratio (fix lost-in-middle for RAG) | `rtrt-compress` + `rtrt-memory` | high |
| [microsoft/LLMLingua](https://github.com/microsoft/LLMLingua) | LLMLingua-2 BERT-tier distilled encoder, 3-6× faster | `rtrt-compress` | medium |
| [yamadashy/repomix](https://github.com/yamadashy/repomix) | Tree-sitter `--compress` mode extracts signatures, drops bodies | `rtrt-compress` | high |
| [yamadashy/repomix](https://github.com/yamadashy/repomix) | Secretlint scan before pack (block secrets from reaching the LLM) | `rtrt-compress` + `rtrt-proxy` | high |
| [yamadashy/repomix](https://github.com/yamadashy/repomix) | Multi-format output (XML / MD / Plain) + per-file token counts | `rtrt-compress` + `rtrt-core` | medium |

## Command-output filtering

| Project | Idea | Fits | Priority |
|---------|------|------|----------|
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | `discover` command scans history for missed reduction wins | `rtrt-cli` + `rtrt-dashboard` | high |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | Per-agent installers (`init --agent cursor/windsurf/...`) | `rtrt-cli` + `rtrt-templates` | high |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | `--ultra-compact` ASCII-icon mode (extra savings tier) | `rtrt-proxy` + `rtrt-compress` | medium |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | `err <cmd>` / `test <cmd>` generic wrappers (errors-only output) | `rtrt-proxy` | medium |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | Session adoption analytics (`rtk session`, `gain --graph`) | `rtrt-dashboard` | medium |
| [Aider-AI/aider](https://github.com/Aider-AI/aider) | Auto-lint + test loop, feed only failures back to the LLM | `rtrt-proxy` + `rtrt-core` | medium |

## Persistent memory

| Project | Idea | Fits | Priority |
|---------|------|------|----------|
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | Multi-level memory tiers (user / session / agent scope) | `rtrt-memory` | high |
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | Hybrid recall: semantic + BM25 + entity linking | `rtrt-memory` | high |
| [mem0ai/mem0](https://github.com/mem0ai/mem0) | Single-pass ADD-only LLM extraction (cheap, low-token) | `rtrt-memory` + `rtrt-providers` | medium |
| [chroma-core/chroma](https://github.com/chroma-core/chroma) | Auto-embed on insert + pluggable embedding function | `rtrt-memory` | high |
| [chroma-core/chroma](https://github.com/chroma-core/chroma) | Collections CRUD + metadata-filter query API | `rtrt-memory` + `rtrt-mcp` | high |
| [letta-ai/letta](https://github.com/letta-ai/letta) | Memory blocks (persona / human / context, structured) | `rtrt-memory` | high |
| [letta-ai/letta](https://github.com/letta-ai/letta) | Context-window manager: overflow → archival via FTS/embed recall | `rtrt-compress` + `rtrt-memory` | high |
| [letta-ai/letta](https://github.com/letta-ai/letta) | Self-editing memory via agent tool calls | `rtrt-memory` + `rtrt-mcp` | medium |
| [cpacker/MemGPT](https://github.com/cpacker/MemGPT) | Tiered memory blocks (human / persona / custom) self-edited | `rtrt-memory` | high |
| [cpacker/MemGPT](https://github.com/cpacker/MemGPT) | Virtual-context paging: hot context ⇄ archival | `rtrt-memory` | high |
| [qdrant/qdrant](https://github.com/qdrant/qdrant) | HNSW ANN index for vectors | `rtrt-memory` | high |
| [qdrant/qdrant](https://github.com/qdrant/qdrant) | Scalar / binary quantization (cut RAM by up to 97%) | `rtrt-memory` | medium |
| [qdrant/qdrant](https://github.com/qdrant/qdrant) | JSON payload filter DSL (range / geo / bool) | `rtrt-memory` | medium |
| [lancedb/lancedb](https://github.com/lancedb/lancedb) | Unified vector + FTS + SQL query surface | `rtrt-memory` + `rtrt-cli` | medium |
| [lancedb/lancedb](https://github.com/lancedb/lancedb) | Columnar Lance format + zero-copy versioning | `rtrt-memory` | low |
| [neuml/txtai](https://github.com/neuml/txtai) | Pipeline / workflow DAG composition | `rtrt-core` + `rtrt-cli` | medium |
| [neuml/txtai](https://github.com/neuml/txtai) | Graph + vector + relational unified store | `rtrt-memory` | medium |
| [Aider-AI/aider](https://github.com/Aider-AI/aider) | Repo-map (rank + prune by graph centrality, tree-sitter tags) | `rtrt-compress` + `rtrt-memory` | high |

## Multi-provider routing

| Project | Idea | Fits | Priority |
|---------|------|------|----------|
| [Helicone/helicone](https://github.com/Helicone/helicone) | Multi-provider gateway with one key | `rtrt-providers` + `rtrt-proxy` | high |
| [Helicone/helicone](https://github.com/Helicone/helicone) | Auto cost / latency / token metrics per request | `rtrt-proxy` + `rtrt-dashboard` | high |
| [Helicone/helicone](https://github.com/Helicone/helicone) | Provider fallback + retry routing | `rtrt-providers` | medium |
| [Helicone/helicone](https://github.com/Helicone/helicone) | Session trace for multi-turn agent flows | `rtrt-dashboard` + `rtrt-mcp` | medium |
| [sobelio/llm-chain](https://github.com/sobelio/llm-chain) | Pluggable multi-model backends behind one trait | `rtrt-providers` | high |
| [sobelio/llm-chain](https://github.com/sobelio/llm-chain) | Reusable prompt templates + chaining primitives | `rtrt-templates` + `rtrt-core` | medium |
| [upstash/context7](https://github.com/upstash/context7) | Version-pinned library-doc fetch via `/org/lib` IDs | `rtrt-providers` + `rtrt-mcp` | high |
| [upstash/context7](https://github.com/upstash/context7) | Dual delivery: MCP server + CLI-skill mode (no MCP needed) | `rtrt-mcp` + `rtrt-cli` | high |
| [upstash/context7](https://github.com/upstash/context7) | OAuth-keyed setup wizard (`rtrt setup` one-shot agent wire-up) | `rtrt-cli` | medium |
| [mufeedvh/code2prompt](https://github.com/mufeedvh/code2prompt) | Git diff/log/branch-compare injection into context | `rtrt-core` + `rtrt-providers` | medium |

## Templates & scaffolds

| Project | Idea | Fits | Priority |
|---------|------|------|----------|
| [mufeedvh/code2prompt](https://github.com/mufeedvh/code2prompt) | Handlebars templates for prompt shaping | `rtrt-templates` | high |
| [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | LangChain-free runtime — pure-Rust, no-Python-dep mirror | `rtrt-core` | high |
| [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | Role / goal / backstory schema for specialised agents | `rtrt-templates` | medium |
| [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI) | Crews + Flows: autonomous agents + deterministic event-driven workflows | `rtrt-core` + `rtrt-templates` | medium |
| [dust-tt/dust](https://github.com/dust-tt/dust) | No-code agent builder UI | `rtrt-dashboard` | medium |
| [dust-tt/dust](https://github.com/dust-tt/dust) | JS SDK + API docs for external integration | `rtrt-core` + `rtrt-dashboard` | medium |

## Observability & cost tracking

| Project | Idea | Fits | Priority |
|---------|------|------|----------|
| [langfuse/langfuse](https://github.com/langfuse/langfuse) | Trace instrumentation for LLM calls | `rtrt-providers` + `rtrt-dashboard` | high |
| [langfuse/langfuse](https://github.com/langfuse/langfuse) | Versioned prompt registry with server cache | `rtrt-templates` + `rtrt-dashboard` | high |
| [langfuse/langfuse](https://github.com/langfuse/langfuse) | Eval datasets + LLM-as-judge scoring | `rtrt-dashboard` | low |
| [Doriandarko/claude-engineer](https://github.com/Doriandarko/claude-engineer) | Live token-budget meter + context-window manager | `rtrt-dashboard` + `rtrt-core` | high |
| [Doriandarko/claude-engineer](https://github.com/Doriandarko/claude-engineer) | Self-generated tools hot-loaded at runtime | `rtrt-mcp` + `rtrt-core` | medium |

## Convergent themes

Multiple sources point in the same direction; RTRT should adopt these early:

1. **Hybrid recall (BM25 + vector + entity / graph)** — mem0, chroma, qdrant, lancedb, letta all converge. Sets the schema target for `rtrt-memory` v0.2/v0.3.
2. **Tree-sitter–aware compression** — repomix and aider both use it. Highest-leverage borrow for `rtrt-compress`; signature-only mode adds another savings tier.
3. **Multi-provider gateway with built-in observability** — Helicone + Langfuse + llm-chain converge. Maps directly onto `rtrt-providers` + `rtrt-proxy` + `rtrt-dashboard`.
4. **Memory tiers + virtual-context paging** — Letta and MemGPT converge. Pairs well with the existing `rtrt-compress` archival pipeline.
5. **Per-agent installers / setup wizard** — rtk and context7 converge. Lowest-friction onboarding for the `rtrt` CLI.

## Immediate adoption candidates (queue for v0.3)

These are the items that:
- map to an existing crate without new dependencies,
- are independently usable,
- and have at least two reference projects suggesting the same shape.

1. **`compress.tree_sitter` mode** in `rtrt-compress` — extract signatures, drop bodies. Sources: repomix + aider.
2. **`memory.recall_hybrid`** in `rtrt-memory` — BM25 + vector + entity, Reciprocal Rank Fusion. Sources: mem0 + chroma + qdrant. (v0.2 already plans BM25 + vector; entity stays for v0.3.)
3. **`providers.gateway`** in `rtrt-providers` — single key in front of many providers, cost/latency metrics per request feed `rtrt-dashboard`. Sources: Helicone + Langfuse + llm-chain.
4. **`rtrt setup --agent <name>`** in `rtrt-cli` — wire RTRT into Claude Code / Cursor / Windsurf / Codex / Aider with one command, mirroring rtk's `init` flow.
5. **`rtrt-compress secretlint` pre-pass** in `rtrt-compress` — block secrets before they reach the LLM. Source: repomix.

## How to read this list

This is an *inspiration backlog*, not a roadmap. The actual roadmap lives in [`README.md`](../README.md#roadmap) and [`CHANGELOG.md`](../CHANGELOG.md). When an item from this page moves into the roadmap or ships in a release, the release note adds an inline `(inspired by [project-name](url))` credit and `THIRD_PARTY_LICENSES.md` gains an entry under "Reference projects".
