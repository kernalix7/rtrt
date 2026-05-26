# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Highlights — local LLM compress model sweep

- `docs/PERF.md` + `docs/PERF.ko.md` publish a length-tiered comparison of local Ollama models on the LLM auto-compress path (20 realistic captures per tier × six tiers, XS ~16 chars to XXL ~6000). Headline: compression ratio is driven by input length far more than the model — short rows barely compress (so `RTRT_AUTO_COMPRESS_MIN_CHARS=512` correctly skips them), dense mid-length sits at ~25-30%, long verbose captures reach 40%+.
- Recommended local default is `gemma3:4b`: robust across every length (XXL 42%), 4.3 GB so it fits 100% on a modest GPU, safely skips short rows. `granite4.1:8b` is flagged unfit for very long captures (returned all 6000-char samples unchanged), `llama3.1:8b` corrupts facts, `qwen3.5:9b` (thinking model) returns input verbatim.
- `docs/USAGE.md` + `docs/USAGE.ko.md` note the `RTRT_AUTO_COMPRESS_MODEL=gemma3:4b` local override; the code default stays `claude-haiku-4-5` for cloud-key users.

### Highlights — MCP Prompts/Resources + ONNX backend + BERTScore

**Three remaining roadmap items land in one sweep. MCP server now exposes the full handler triad (tools / prompts / resources); the heuristic `MlCompressor` graduates with an optional real ONNX-runtime backend matching the LLMLingua-2 contract; `rtrt-eval` gains a BERTScore evaluator behind the same encoder-loading machinery. All new code is feature-gated and ships zero model files.**

- `rtrt-mcp` declares `enable_prompts()` + `enable_resources()` and implements the four handlers. `prompts/list` enumerates every name in the local `PromptRegistry` (default `~/.rtrt/prompts/`, override with `RTRT_PROMPTS_DIR`); `prompts/get` returns the latest version with handlebars argument substitution. `resources/list` surfaces one `memory://<project>/timeline` per project plus one `memory://<project>/block/<name>` per Letta block; `resources/read` returns either JSON-Lines timeline rows or the block body. Errors are mapped to `McpError::invalid_params` / `internal_error` and never crash the server.
- New `rtrt-templates::render::render_str` makes the handlebars renderer public so MCP and any other consumer share the same `{{var}}` engine the scaffolder uses.
- `rtrt-compress::OnnxImportance` — opt-in `onnx` feature pulls `ort = 2.0.0-rc.12` (`load-dynamic`), HuggingFace `tokenizers`, and `ndarray`. `MlCompressor::onnx(model, tokenizer)` constructs a session, runs the user-supplied model on `input_ids` + `attention_mask`, and maps the per-subword keep-probability back to whitespace-tokens via the tokenizer's offsets. Default build does not link `ort` — workspace size stays the same for users who only want the rule engine.
- New CLI plumbing: `rtrt compress --ml --onnx-model <path> --onnx-tokenizer <path>` (gated by `rtrt-cli --features onnx`, forwards to `rtrt-compress/onnx`). Both env vars (`RTRT_ONNX_MODEL` / `RTRT_ONNX_TOKENIZER`) accepted.
- `rtrt-eval::bertscore` — opt-in `bertscore` feature. `BertScoreScorer::new(encoder.onnx, tokenizer.json)` builds an L2-normalised per-subword embedder; `score(reference, hypothesis)` returns greedy-aligned `(P, R, F1)`; `evaluate_fixture(fixture, level)` runs the compressor and reports per-sample + mean scores. CLI: `rtrt-eval bertscore --model ... --tokenizer ... [--level full]`.
- `docs/USAGE.md` + `docs/USAGE.ko.md` document the ONNX model contract, the BERTScore workflow, and the env vars / feature flags for both surfaces. README roadmaps (EN + KO) flip the three items to done and drop the deferred multi-agent line to its own bullet.

### Highlights — rtrt-eval opt-in harness

**Tenth workspace crate `rtrt-eval` ships. Two surfaces (recall quality + compression ratio) reduce a JSON fixture into a single number you can put on a dashboard. The built-in smoke fixture is intentionally tiny; the harness accepts external fixtures with the same shape so LongMemEval-S or an in-house corpus plugs in without code changes. R@5 = 0.857 + MRR = 0.857 on the smoke corpus, enforced by an in-crate floor test.**

- New crate `crates/rtrt-eval/`: library + `rtrt-eval` binary. Subcommands `recall` and `compress`, JSON or human output, `--fixture <path>` to override the built-in smoke set.
- Library API: `RecallFixture`, `CompressFixture`, `evaluate_recall(&fixture, k) -> RecallReport`, `evaluate_compression(&fixture, level) -> CompressReport`. Embedded fixtures published as `RECALL_SMOKE` / `COMPRESS_SMOKE` consts.
- Smoke fixtures `crates/rtrt-eval/fixtures/recall_smoke.json` (12 docs, 7 hand-labelled queries) + `compress_smoke.json` (3 prose samples). Hand-tuned so BM25 should clear the R@5 ≥ 0.80 floor; failure to clear blocks merges via the `recall_at_5_on_smoke_fixture_clears_floor` test.
- `docs/PERF.md` + `docs/PERF.ko.md` publish the first measured numbers from the smoke fixture. Marked explicitly as smoke (not a competition benchmark) — real numbers require a real labelled corpus.
- README roadmap (EN + KO): rtrt-eval and the smoke script flipped to done; BERTScore numbers, ONNX backend, and tagged release remain open.

### Highlights — LLM auto-compress + live-key smoke gate

- Opt-in LLM compression daemon in `rtrt-dashboard`. With `RTRT_AUTO_COMPRESS_LLM=1` set, a background tokio task sweeps rows older than `RTRT_AUTO_COMPRESS_AGE_SEC` whose body exceeds `RTRT_AUTO_COMPRESS_MIN_CHARS`, asks the configured gateway model (`RTRT_AUTO_COMPRESS_MODEL`, default `claude-haiku-4-5`) to rewrite each one losslessly-of-meaning, and writes the result back. Rewritten rows are tagged with `metadata.compressed_at` / `compressed_model` / `compressed_from_chars` / `compressed_to_chars` so the next sweep skips them. Rows where the model produces empty or non-shrinking output are marked `compressed_skip=no-shrink` and left untouched.
- New `MemoryStore::set_body` (FTS5-synced overwrite via `'delete' + insert` on the external-content index) and `MemoryStore::compress_candidates` (age / min-chars / not-yet-compressed filter) — the primitives the daemon stands on. Regression-covered by `auto_compress_primitives`.
- `scripts/smoke.sh` — live-key smoke harness. Runs `rtrt --version` / `compress` / `proxy` / `templates` / `new` / `repo-map` unconditionally; runs Anthropic / OpenAI / OpenAI-compatible provider chats when the matching env vars are present (otherwise SKIP); spawns `rtrt-dashboard` + `rtrt-mcp` on loopback ports and probes `/healthz`, `/api/templates`, `/api/stats`, plus MCP HTTP reachability and the bearer guard's 401. Exits non-zero only when a check that actually ran failed. Designed as the gate before promoting `0.1.0` to a tagged release.
- `docs/USAGE.md` + `docs/USAGE.ko.md` document the seven `RTRT_AUTO_COMPRESS_*` env knobs and the metadata fields the daemon writes.

### Highlights — Dashboard / docs / regression coverage

- Dashboard activity feed subscribes to `/api/stream` via `EventSource` and only falls back to 5-second polling when SSE is unavailable. Captures now show up live without refreshing.
- `docs/USAGE.md` + `docs/USAGE.ko.md` document the full 18-tool MCP surface (`memory_timeline` / `memory_profile` / `memory_relations` / `memory_smart_search` / `memory_export` / `memory_consolidate` / `memory_sessions` / `repo_map` added to the table) and the four `RTRT_AUTO_*` env knobs that the MCP server honours. Korean USAGE also gains the dashboard auto-capture pipeline section that the English doc already had.
- `rtrt-memory` regression test `auto_capture_pipeline_primitives` verifies the building blocks the dashboard and MCP both depend on: deterministic `body_sha`, `body_seen_at` dedup window (per-project scoping), `tag_row` session + sha writes, `sessions` / `session_records` grouping, and `archive_overflow_no_llm` newest-N retention.

### Highlights — Direction refresh follow-ups

**Schema v5 lands a covering index for the timeline pager (`recent_paged` p50 on 100 K rows dropped from 71 ms to ~32 µs — 2200×). The Claude Code plugin now wires twelve hooks instead of six. MCP gains a seventh memory tool (`memory_sessions`) that exposes the v4 `session_id` column, and four MCP tool handlers (`compress` / `compress_ml` / `proxy` / `provider_chat`) now run through the same auto-capture pipeline the dashboard uses. A PR-time perf gate (`.github/workflows/perf.yml` + `scripts/perf-gate.sh`) refuses any benchmark that regresses beyond 10 % of the baseline. Korean README is back in sync with the Unix-toolkit positioning.**

- `rtrt-memory` schema v5: `idx_memories_timeline` covering `(project, created_at DESC, id DESC)`. New `sessions()` + `session_records()` helpers group rows by `session_id` for replay / export. `recent_paged` p50 is now sub-50 µs at every size we bench.
- `rtrt-mcp` adds `memory_sessions` (per-project session summary or per-session row list) bringing the server to 18 tools. `RtrtState` grows a `auto_capture()` helper that mirrors the dashboard pipeline (`redact_secrets` → SHA-256 dedup → save → session tag); `compress`, `compress_ml`, `proxy`, and `provider_chat` all run it on success. Env knobs: `RTRT_AUTO_CAPTURE` / `RTRT_AUTO_REDACT` / `RTRT_AUTO_DEDUP_WINDOW_SEC` / `RTRT_DEFAULT_PROJECT` (same as the dashboard).
- Claude Code plugin (`plugins/claude-code/rtrt/`) now ships twelve hooks: PreToolUse / PostToolUse / PostToolUseFailure / PreCompact / UserPromptSubmit / PostUserPromptSubmit / Notification / Stop / SubagentStart / SubagentStop / SessionStart / SessionEnd.
- `.github/workflows/perf.yml` benches `rtrt-memory` against the PR base ref with `--save-baseline` / `--baseline`, then `scripts/perf-gate.sh` parses criterion's `estimates.json` and exits non-zero on >10 % p50 regression. Documents the policy already in `docs/PERF.md`.
- `docs/PERF.md` + `docs/PERF.ko.md` updated with the post-v5 measurements.
- `docs/README.ko.md` rewritten to match the Unix-toolkit positioning, three-pillar block, DESIGN/PERF links, and the 18-tool MCP surface.

### Highlights — Direction refresh

**RTRT formally commits to a Unix-philosophy toolkit. New top-level `DESIGN.md` documents the ten principles; new `docs/PERF.md` publishes the SLO table and the first measured numbers. Auto-capture is no longer optional ceremony — every dashboard `/api/*` call and every Claude Code hook fire runs through a SHA-256 dedup + privacy filter + session tag pipeline before landing in SQLite. An hourly consolidation daemon keeps each project under a row cap. Six new memory MCP tools (timeline / profile / relations / smart_search / export / consolidate) plus a Server-Sent Events live stream + a tokens aggregator close the gap with broader memory platforms while staying narrow.**

- New `DESIGN.md` + `docs/DESIGN.ko.md`: ten short principles — tools-not-frameworks, stable substrates, three pillars only, auto-capture by default, optional crates instead of scope creep, measured performance, local-first, frozen public interfaces, slow + deep growth. Re-read before adding a top-level feature.
- New `docs/PERF.md` + `docs/PERF.ko.md`: SLO table + last-measured numbers from `recall_bench`. Regression beyond 10% blocks a release.
- `rtrt-memory` schema v4 lands `session_id` + `body_sha` columns + their indexes. `body_sha()` / `body_seen_at()` / `tag_row()` / `archive_overflow_no_llm()` round out the auto-capture pipeline.
- `rtrt-dashboard` auto-capture pipeline: every successful `/api/{chat,compress,diagnose,proxy}` now runs `redact_secrets` → SHA-256 dedup (5-minute default window) → save → session id tag. Env knobs: `RTRT_AUTO_CAPTURE` / `RTRT_AUTO_REDACT` / `RTRT_AUTO_DEDUP_WINDOW_SEC` / `RTRT_DEFAULT_PROJECT`. Each save broadcasts a JSON event over `/api/stream` (SSE) so clients can subscribe instead of polling.
- Hourly consolidation daemon — `spawn_consolidation_daemon` runs `archive_overflow_no_llm` per project, keeps `RTRT_CONSOLIDATE_KEEP` (default 1000) most recent rows. Cadence via `RTRT_CONSOLIDATE_INTERVAL_SEC` (default 3600, 0 disables).
- `GET /api/memory/projects` + `GET /api/memory/timeline?project=X&limit=N&offset=M` power the dashboard project picker and paginated history.
- `GET /api/tokens/summary` aggregates the gateway's request history into hourly + daily buckets.
- `GET /api/stream` SSE channel + 256-slot tokio broadcast — `{type: "memory.save", id, kind, project, session}` events fire on every capture.
- Six new MCP memory tools: `memory_timeline` (paginated history), `memory_profile` (per-project stats), `memory_relations` (graph BFS), `memory_smart_search` (BM25 today, hybrid when an embedder is attached), `memory_export` (JSONL), `memory_consolidate` (LLM-free archive). MCP server now ships 17 tools across memory / token / code / project / LLM domains.
- `plugins/claude-code/rtrt/` — Claude Code plugin scaffold with six hook scripts (PreToolUse / PostToolUse / UserPromptSubmit / Stop / SessionStart / SessionEnd). Writes via `rtrt` CLI when available, falls back to `POST /api/memory/save` against a running dashboard. Best-effort: never blocks the agent on capture failure.
- `crates/rtrt-memory/benches/recall_bench.rs` — criterion bench across `recall_bm25` / `recent_paged` / `save_one` / `projects_listing` at 1 K / 10 K / 100 K rows. First published numbers in `docs/PERF.md`.
- Workspace deps: `sha2` (memory dedup), `uuid` (session ids), `tokio-stream` (SSE BroadcastStream).

### Highlights — Earlier in this branch

- `rtrt-mcp` adds a Streamable HTTP transport (MCP 2025-06-18) via `rmcp::StreamableHttpService` behind an axum router. New tools: `compress_ml` (LLMLingua-style token-importance compression), `proxy` (rtrt-proxy filters), `memory_set_block` / `memory_get_block` / `memory_list_blocks` (Letta-style persona / human / context slots), and a `filter` parameter on `memory_recall` for qdrant-style payload DSL. `--http-token` / `RTRT_MCP_HTTP_TOKEN` enforces a constant-time bearer guard with `WWW-Authenticate`; `--allowed-origins` plumbs into `StreamableHttpServerConfig.allowed_origins` for RFC 6454 Origin validation. Non-loopback bind without a token logs a warning. (inspired by [letta](https://github.com/letta-ai/letta), [Helicone](https://github.com/Helicone/helicone))
- `rtrt-memory` gains a `metadata` column (v3 migration) and a qdrant-style payload filter DSL: `source=claude,topic~^auth` (key=val, key!=val, key~regex, comma-AND). `recall_bm25_with_filter`, `save_with_metadata`, `get_metadata` / `set_metadata` round out the API. `export_jsonl` / `import_jsonl` provide a portable backup format keyed off the public schema. (inspired by [qdrant](https://github.com/qdrant/qdrant))
- `rtrt-providers` ships a Helicone-style response cache on `Gateway` via `with_cache(cap)` — cache key is `(model, messages, max_tokens, temperature)`; hits skip retries, metrics, and the budget meter. (inspired by [Helicone](https://github.com/Helicone/helicone))
- `rtrt-compress` gets an LLMLingua-style scaffold (`MlCompressor` + `TokenImportance` trait + `HeuristicImportance` baseline; ONNX backend deferred), chroma-style multi-format output (`compress_to(Plain|Markdown|Xml|Json)`), and tree-sitter grammars for Python and TypeScript on top of the existing Rust grammar. (inspired by [LLMLingua](https://github.com/microsoft/LLMLingua), [chroma](https://github.com/chroma-core/chroma))
- `rtrt-templates` adds a built-in `agent-role` template (crewAI-style role / goal / backstory triad + tool list); the dashboard exposes the full registry over `/api/templates/scaffold`. (inspired by [crewAIInc/crewAI](https://github.com/crewAIInc/crewAI))
- `rtrt-dashboard` doubles in surface: 10 tabs (Metrics / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup) with SVG sparklines for latency + tokens, dark/light toggle, parent_id-grouped retry trace tree, and routes `/api/{prompts*, budget, memory/recall, memory/save, memory/blocks*, compress, proxy, diagnose, repo-map, setup}`. `RTRT_DASHBOARD_TOKEN` enables a bearer-token middleware on every `/api/*` path. (inspired by [langfuse](https://github.com/langfuse/langfuse), [Helicone](https://github.com/Helicone/helicone))
- `rtrt-cli`: new subcommands `rtrt diagnose <cmd>` (aider-style failure triage), `rtrt mcp [--transport]` (passthrough to `rtrt-mcp`), `rtrt benchmark` (cargo bench wrapper), `rtrt memory export/import`. Existing `rtrt compress` learns `--ml --ratio` and `--format {plain|markdown|xml|json}`; `rtrt memory recall` learns `--filter`; `rtrt signatures` learns `--lang python|typescript`. (inspired by [aider](https://github.com/Aider-AI/aider))
- First-class langfuse-style versioned prompt API on the dashboard: GET `/api/prompts`, `/api/prompts/{name}`, `/api/prompts/{name}/{version}` driven by the existing `PromptRegistry`.

**First sweep from `[Unreleased]` history (kept for traceability) — twelve HIGH-priority items: memory tiers / edges-graph / HNSW, gateway budget meter + per-request traces, prompt registry, context7 doc fetcher, repo-map + signature extractor, `rtrt discover`, handlebars templating, rule-pass extensions + LLM compression mode.**

- `rtrt-providers` chat + streaming against Anthropic and OpenAI; OpenAI-compatible adapter covers Ollama / llama.cpp / vLLM / LM Studio. Usage is parsed for both providers and flows into the dashboard. New `Gateway` fronts every provider behind one entry point and records per-request `RequestMetric { id, parent_id, provider, model, started_at, latency_ms, usage, cost_usd, ok }`; `Gateway::with_budget(Budget::new(USD))` fails-fast when cumulative cost exceeds the cap. (inspired by [Helicone](https://github.com/Helicone/helicone), [llm-chain](https://github.com/sobelio/llm-chain), [langfuse](https://github.com/langfuse/langfuse), [Doriandarko/claude-engineer](https://github.com/Doriandarko/claude-engineer))
- `rtrt-providers` `Context7Client` fetches version-pinned library docs from `https://context7.com/api/v1/<owner>/<repo>`; `rtrt docs facebook/react --topic hooks` is the CLI surface. (inspired by [upstash/context7](https://github.com/upstash/context7))
- `rtrt-mcp` ships a real stdio MCP server via [`rmcp`](https://crates.io/crates/rmcp) 1.x exposing `compress`, `memory_save`, `memory_recall`, `templates_list`, `templates_scaffold`.
- `rtrt-memory` adds local `all-MiniLM-L6-v2` embeddings (`fastembed`, behind the `embeddings` feature) plus BM25 + vector hybrid recall via Reciprocal Rank Fusion. New `MemoryScope` tiers (`User` / `Agent` / `Session` / `Project`) with `save_scoped` + `recall_bm25_scoped`. `add_edge` / `recall_via_graph` walk the `edges` table with BFS depth control. `MemoryStore::with_embedder(Arc<dyn Embedder>)` auto-embeds on every `save`. Behind the new `hnsw` feature, `HnswIndex` provides sub-linear ANN recall over the per-project embedding set via `instant-distance`. `archive_overflow` aliases `compress_project` for the Letta / MemGPT context-overflow → archival framing. (inspired by [mem0](https://github.com/mem0ai/mem0), [chroma](https://github.com/chroma-core/chroma), [qdrant](https://github.com/qdrant/qdrant), [letta](https://github.com/letta-ai/letta), [MemGPT](https://github.com/cpacker/MemGPT), [agentmemory](https://github.com/rohitg00/agentmemory))
- `rtrt-memory` ships the `Summariser` trait + `LlmSummariser` (behind the `llm` feature) so memory extraction and compression work with any provider — including a local Ollama server through the existing OpenAI-compatible adapter, no new HTTP code. `rtrt memory extract` and `rtrt memory compress` CLI commands expose the flow. (inspired by [mem0](https://github.com/mem0ai/mem0) ADD-only extraction, [MemGPT](https://github.com/cpacker/MemGPT) virtual-context paging)
- `rtrt-compress` `criterion` benchmark harness — the README's compression-savings claim is now testable. New `Extreme` level. Rule pack extended with hedging (`I think`, `perhaps`, …), discourse markers (`moreover`, `however`, …), meta-phrases (`it is important to note that`, …), and verbose-qualifier removal at the extreme level. `secrets::redact_secrets` pre-pass scrubs AWS / GitHub / OpenAI / Anthropic / Slack / Bearer / `api_key=…` / PEM private-key blocks before any rule fires. `LlmCompressor` (behind `llm-compress` feature) routes through any provider — Anthropic, OpenAI, or local Ollama — for caveman-class 50–75% savings. Tree-sitter signature extractor for Rust under the `treesitter` feature; 78% byte reduction measured on a real `rtrt-providers` source file. (inspired by [caveman](https://github.com/JuliusBrussee/caveman), [repomix](https://github.com/yamadashy/repomix), [aider](https://github.com/Aider-AI/aider))
- `rtrt-templates` switches `{{var}}` substitution to `handlebars` so templates can use conditionals (`{{#if foo}}…{{/if}}`) and loops (`{{#each items}}…{{/each}}`) on top of the existing variable pass. New `prompts` module + `PromptRegistry` stores versioned prompts under `<root>/<name>/<NNNN>.toml`; CLI surfaces it as `rtrt prompt {save,get,list,versions}`. (inspired by [code2prompt](https://github.com/mufeedvh/code2prompt), [langfuse](https://github.com/langfuse/langfuse))
- `rtrt-cli` gains `rtrt discover`, `rtrt repo-map`, `rtrt signatures`, `rtrt setup`, `rtrt docs`, `rtrt prompt`. `discover` scans `~/.zsh_history` / `~/.bash_history` for commands that match a `rtrt_proxy` filter and reports top-N matches. `repo-map` walks a directory and emits a compressed tree-sitter signature map sorted by signature size. `setup --agent <name>` writes the MCP config for Claude / Cursor / Codex / Windsurf with a `.bak` safety net. (inspired by [rtk](https://github.com/rtk-ai/rtk), [aider](https://github.com/Aider-AI/aider))
- `install.sh` + `install.ps1` one-liners wired to GitHub Releases with SHA256 verification; `release.yml` builds 5 targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`), attaches them to the GitHub Release, and publishes all 9 crates to crates.io on a `REL-vX.Y.Z` marker tag.
- `cargo-deny` license + advisory + bans + sources gate, blocking on PRs to `main` and on a weekly cron.
- New [`docs/INSPIRATION.md`](docs/INSPIRATION.md) — 50+ borrow ideas from 18 reference projects mapped to specific RTRT crates with priority.

### Added

- **MCP HTTP transport**: `--transport http --bind ADDR --path /mcp` boots `rmcp::StreamableHttpService` behind an axum `Router`. `--http-token` enforces a constant-time bearer guard. `--allowed-origins` plumbs `StreamableHttpServerConfig.allowed_origins`.
- **MCP tools**: `compress_ml`, `proxy`, `memory_set_block` / `memory_get_block` / `memory_list_blocks`, `filter` parameter on `memory_recall`.
- **Memory payload filter DSL**: `PayloadFilter::parse("source=claude,topic~^auth")`, `recall_bm25_with_filter`, `save_with_metadata`, `get_metadata`, `set_metadata`; v3 schema migration adds the `metadata` column.
- **Memory backup**: `MemoryStore::export_jsonl` / `import_jsonl`; CLI `rtrt memory export --project --out` / `rtrt memory import --in`.
- **Provider cache**: `Gateway::with_cache(cap)` + `cache_len`; cache key is `(model, messages, max_tokens, temperature)`.
- **ML compress scaffold**: `rtrt_compress::MlCompressor` + `TokenImportance` trait + `HeuristicImportance` baseline + `CompressionTarget::new(ratio)`. CLI `--ml --ratio`. MCP `compress_ml`. Dashboard Compression tab.
- **Multi-format compress**: `Compressor::compress_to(OutputFormat::{Plain|Markdown|Xml|Json})` with CDATA-escape guard.
- **Tree-sitter Python + TypeScript**: `Language::{Python, TypeScript}` + body-stripping walkers; CLI `rtrt signatures --lang {python|typescript}`.
- **agent-role template**: crewAI-style role / goal / backstory triad + `agent.toml` + `system_prompt.md`.
- **Dashboard**:
  - 10 tabs: Metrics / Budget / Prompts / Memory / Templates / Compression / Proxy / Diagnose / RepoMap / Setup. Dark / light toggle (CSS variables + `prefers-color-scheme` + `localStorage`).
  - SVG sparklines (latency, tokens) on the Metrics tab; retry-chain rows grouped by `parent_id`.
  - Routes: `/api/prompts*`, `/api/budget`, `/api/memory/{recall,save,blocks,blocks/{name}}`, `/api/compress`, `/api/proxy`, `/api/diagnose`, `/api/repo-map`, `/api/setup`.
  - `RTRT_DASHBOARD_TOKEN` enables a bearer-token middleware on every `/api/*`; `/`, `/healthz`, `/favicon.ico` stay open. Non-loopback bind without a token logs a warning.
- **CLI**: `rtrt diagnose`, `rtrt mcp [--transport]`, `rtrt benchmark`, `rtrt memory export` / `rtrt memory import`. New flags on existing commands: `compress {--ml --ratio --format}`, `memory recall --filter`, `signatures --lang {python|typescript}`.
- **Gateway**: `budget_cap_usd`, `budget_spent_usd` accessors for the dashboard.

- `rtrt-providers`: real `chat()` + `chat_stream()` against Anthropic and OpenAI; `OpenAICompatibleProvider` with user-supplied base URL; shared SSE decoder; `Usage { input_tokens, output_tokens, cache_read, cache_creation }` with `merge` / `total`; `ChatStreamEvent::{ Delta, Usage, Done }`; `Gateway` + `Budget` + `ModelPricing` + `RequestMetric { id, parent_id, cost_usd, … }` + `MetricsView`; `Gateway::from_env`, `Gateway::with_budget`, `Gateway::chat_with_parent`; `Context7Client::get_library_docs(library, topic)`.
- `rtrt-cli`: full subcommand set — `compress {-l, --llm}`, `proxy`, `templates`, `new`, `provider chat`, `memory {save,recall,extract,compress}`, `prompt {save,get,list,versions}`, `signatures`, `repo-map`, `discover`, `docs`, `setup --agent <name>`.
- `rtrt-memory`: `Embedder` trait, `FastEmbedder` (`embeddings` feature, `all-MiniLM-L6-v2`), `MemoryScope` enum, `save_scoped`, `recall_bm25_scoped`, `recall_vector`, `recall_hybrid` (Reciprocal Rank Fusion, `rrf_k = 60`), `add_edge` / `delete_edge` / `recall_via_graph`, `list_by_project`, `delete`, `Summariser` trait, `LlmSummariser` (`llm` feature), `extract_and_save`, `compress_project`, `archive_overflow`, `MemoryStore::with_embedder` (auto-embed on `save`), `HnswIndex` (`hnsw` feature, `instant-distance`).
- `rtrt-compress`: criterion benches across `lite` / `full` / `ultra` / `extreme` × 4 fixtures; `secrets::redact_secrets` pre-pass for 10 secret shapes; `LlmCompressor` (`llm-compress` feature) wrapping any `Provider`; `SignatureExtractor` for Rust (`treesitter` feature); `scripts/bench.sh` prints the savings table.
- `rtrt-templates`: `prompts` module + `PromptRegistry` + `Prompt`; handlebars-backed `render::substitute` so templates can use conditionals + loops.
- `rtrt-mcp`: 6 tools over rmcp stdio (`compress`, `memory_save`, `memory_recall`, `templates_list`, `templates_scaffold`, `provider_chat`); `--memory` flag selects the SQLite store; logs to stderr.
- `install.sh` + `install.ps1`: detect OS+arch, resolve latest release, download tarball/zip, SHA256-verify, drop binaries to `~/.local/bin` (Linux/macOS) or `%LOCALAPPDATA%\Programs\rtrt\` (Windows). `--main` fallback builds from source, `--uninstall` removes the three binaries.
- `.github/workflows/release.yml`: tagged-release builds 5-target matrix, extracts the CHANGELOG section on `REL-` tags, publishes crates.io in dependency order.
- `.github/workflows/deny.yml`: blocking `cargo deny check licenses sources bans advisories` on every push/PR/weekly cron.
- `deny.toml`: license allowlist (MIT, Apache-2.0, BSD-{2,3}-Clause, ISC, MPL-2.0, Unicode-3.0, Zlib, BSL-1.0, OpenSSL exception for `ring`); copyleft denied.

### Changed

- `rtrt-core`: `CompressionLevel` and `Config` switch to `#[derive(Default)]` with `#[default]` enum variant; manual impls removed (clippy `derivable_impls`).
- `rtrt-providers` workspace deps add `eventsource-stream`, `futures-util`, `mockito`.
- `Cargo.toml` adds workspace deps for `rmcp`, `schemars`, `criterion`, `fastembed`, `eventsource-stream`, `futures-util`, `mockito`, `tree-sitter`, `tree-sitter-rust`, `instant-distance`, `handlebars`.
- `rtrt-memory` schema gains a `scope` column on `memories` via a `PRAGMA user_version`-gated `ALTER TABLE` migration. Existing databases pick up the column with default `'project'` on first open.

### Fixed

- AIPS plugin workaround at init time: `lib/detect-project.sh` emits unquoted multi-word values (e.g. `DEPLOYMENT=GitHub Actions`), which breaks `lib/render-claude-md.sh`'s `eval` call. Worked around locally.
- `rtrt-cli` clippy fixes on stable: `sort_by(|a,b| b.cmp(a))` → `sort_by_key(Reverse(...))`; manual `if zero { 0 } else { x*100/y }` → `checked_sub` + `checked_mul` + `checked_div` chain.

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
