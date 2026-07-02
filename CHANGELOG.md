# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **MCP semantic recall**: `rtrt-mcp` now builds an `OllamaEmbedder` at startup (embeddings enabled + a cheap reachability probe) and routes `memory_recall` / `memory_smart_search` through hybrid (BM25 + vector RRF) recall once a project has meaningful embedding coverage, sharing the CLI hook's gate/timeout/fallback logic via new `rtrt_memory::hybrid_recall_ready` / `hybrid_embedder_from_config` helpers. `rtrt setup` turns `[embeddings] enabled` on by default when a local Ollama with an embedding-capable model is detected (never overrides an explicit user setting). `memory save` (CLI) and `memory_save` (MCP) now opportunistically embed a small, backlog-scaled batch of unembedded rows after every save (`MemoryStore::opportunistic_embed_sweep`), so coverage climbs even without the dashboard's auto-embed daemon running.

### Changed

- `memory_recall` / `memory_smart_search` MCP tool descriptions and server instructions now describe actual runtime behavior (hybrid when an embedder is attached, BM25 otherwise) instead of a static claim.

### Highlights — dashboard UX overhaul (dead spots fixed, IA tightened)

**Every dashboard surface now does something real: the Sessions decoy became a feature, Route merged into Router, the gateway cards got a live data source, and the last dead panels were removed or wired.**

- **Memory › Sessions is real**: new `GET /api/memory/sessions` (per-session summary rows with count + first/last activity; `?session=<id>` returns that session's memories). The Sessions nav/subtab — previously an alias of Timeline — now renders a session table that drills into per-session memories (rows open the shared detail modal). Deep-linkable at `/memory/sessions`.
- **Route merged into Router**: the near-duplicate Route page is gone; the Router's Routing Preview gained an optional task prompt (prompt → `GET /api/route` dry-run, empty → `GET /api/route/preview`). Old `/route` deep links redirect to `/router`.
- **Chat playground (Tools › Chat)**: a minimal gateway playground over `POST /api/chat` — model select (shared `/api/models` cache), conversation thread, per-reply provider/token/latency meta. This gives the Overview gateway cards (Response Trend / Recent Calls / Budget) a first-party data source; until a call is made they collapse into one honest "Gateway inactive" empty state that links to the playground.
- **Dead surfaces removed/wired**: unreachable Environment-info panel (never-populated auth-token row) deleted; Diagnose's hand-typed model id replaced with the same populated model select the rest of the UI uses; template cards gained a Scaffold action (the generate-project modal was previously unreachable).
- **Navigation**: ⌘K palette now covers every page and subpage in both modes (Sessions, Router, Limits, Chat, Security subtabs included); `/security/scan|profiles` deep links activate the right subtab; Statusline + Capture/Config re-homed under a neutral "Settings" group; Setup page names the real clients (Claude Code / Cursor / Codex CLI).
- **Polish**: installed-model dates use the browser locale (was hard-coded), the topbar Memory pill reflects the savings source's actual availability, and the Recent Calls empty state points at the playground.

### Highlights — dashboard URL routing / deep-linking

**Every dashboard page is now a real URL.**

- History-API routing: each page (and subtab) maps to a path, so refresh, back/forward, and deep links land on the right view; unknown paths fall back to the SPA shell (#56).
- The Memory page's double navigation (sidebar entry + in-page subtabs) is reconciled into a single navigation model (#55).

### Highlights — usage-aware provider routing: ledger → headroom → failover

**The router now knows how much each provider has been used and how much quota is left, prefers targets with headroom, and falls over automatically when a target is exhausted or erroring.**

- **Provider usage ledger** (#52): every provider invocation appends `epoch_ts / target / model / input_tokens / output_tokens / est / ok` to `~/.rtrt/provider-usage.tsv` (`RTRT_PROVIDER_USAGE_PATH` override; capped at the most-recent 5000 rows; writes are best-effort and never fail the call). CLI shell-outs report no real usage, so their token counts are ~chars/4 estimates and stay marked `est` end to end.
- Rolling **5h / 24h / 7d windows** per target; new `rtrt usage` prints the per-target table (estimated rows marked `~`). Headroom = 24h usage against the `[limits.<target>] daily_tokens / daily_requests` caps in `~/.rtrt/config.toml`; targets without a `[limits]` entry report no cap — never a fabricated ceiling.
- **Headroom-weighted selection** (#53): within the local-free → subscription-flat → API-metered cost ordering, a candidate whose scarcest dimension is under ~15% remaining is penalized inside its cost tier, and a fully exhausted target sinks to last resort regardless of tier; ties break toward the roomiest target. `rtrt route --explain` shows the decision, ranked alternatives, and the headroom that drove it.
- **Automatic failover** (#53): `rtrt route --failover` / `rtrt call --failover` walk the ranked candidate list, falling over on retryable failures (rate-limit / quota / 429 / 5xx / timeout) and stopping on terminal errors; the result summarises which targets fell over and why.
- **Dashboard gauges + routing preview** (#54): `GET /api/usage` (windowed usage + headroom per target) and `GET /api/route/preview` (the load-balancing decision for the *next* request, no prompt needed) power usage/headroom gauges and a routing preview on the Tools side.

### Highlights — two-tier config: global base kernel + per-project overrides

**One global base (hooks / MCP / statusline wiring in `~/.rtrt/config.toml`, managed by `rtrt setup`) plus a thin per-project customization file. Effective config = global ⊕ project.**

- `ProjectConfig` at `<repo>/.rtrt/config.toml` (#34, #44): optional overrides for output level (`off`/`lite`/`full`/`ultra`), compression, per-project agent and provider enablement, and the statusline. Absent fields inherit the global value; an all-default override file is deleted so the repo stays clean. `Config::load_effective(repo)` layers the overlay onto the global base.
- Dashboard: every per-project settings surface gained a **Follow global / Custom** scope toggle — statusline (#42), Output Optimizer level (#43), providers / compression / agents (#45).
- `rtrt migrate` (dry-run by default; `--apply` to write) migrates an existing repo to the rtrt project standard; `rtrt project refresh` is the one-command alias (render the project contract → activate canonical settings → audit whole-project consistency). Both detect and strip project-level rtrt-owned key shadows — e.g. a project `.claude/settings.json` re-declaring `statusLine` — with a `.bak` backup, so projects defer to the global base kernel (#34).
- `rtrt project status / health / repair` inspect and repair the standardization contract; team agents install alongside (#32).

### Highlights — per-pillar statusline savings model

**Line 3 of the rich statusline reports per-project savings per pillar, each with an honest unit — never a blended or fabricated percentage.**

- `📝opt:<level>` — Output Optimizer shown as its active terse level (prompt-injection has no before/after to measure, so no percent); `🧠mem:X%` — Memory storage reduction (original vs stored body; internal efficiency, its own pillar); `⚡cmd:Y%` — Command Optimizer EFFECTIVE reduction over runs that actually filtered (passthroughs excluded so they don't dilute); `💯Σ:Z%` — agent-token savings: tokens kept out of the model context (command filtering + recall reuse; storage compression and terse mode excluded) (#37, #38, #39).
- Statusline ctx% corrected and rate-limit windows now reflect the real 5h / weekly windows (#36); labels are rtrt-native (#41); per-project statusline inherit/override, following global by default (#42).

### Highlights — dashboard restructure (Project / Tools)

- Top bar split into **Project / Tools** modes (#51); Overview / Environment / Route pages restructured around the data model (#49); Overview Σ now uses the agent-token model instead of the blended total (#50).
- All remaining config became editable in the web UI (#48).
- Frontend modularized into an HTML shell + `styles.css` + classic-JS modules (#46); backend `main.rs` split into a state / routes / handlers module tree (#47); UI overhauled around a function-grouped IA with unified pages (#40); dashboard v2: 6-section IA, time windows, effective% / coverage, accurate Output Optimizer reporting (#28, #29).

### Highlights — orchestrator: detect → invoke → route

- `rtrt detect` scans the environment for local AI CLIs / APIs / servers and reports what is available, with per-tool opt-in/out surfaced in the dashboard Environment tab (#19, #22).
- `rtrt call <target>` cross-tool invoke bridge: run any detected agent or provider through one prompt interface with `--mode` cli / api / auto selection; also exposed as the MCP `agent_call` tool (#21, #24).
- `rtrt route` cost-aware route selection: pick the cheapest useful target (local-free → subscription → API-metered) for a capability, with `--explain` / `--dry-run`; MCP `agent_route` tool; Orchestration view in the dashboard (#23, #24, #26).
- Rich customizable statusline `rtrt statusline --rich` (model / ctx% / rate-limit windows / per-project savings), customizable from the web UI (#25, #27).

### Highlights — templates, project lifecycle, standardization

- Visual template editor + custom-template CRUD in the dashboard (#30); Template page overhaul — grouped library + polished editor (#33).
- New standardization template + `rtrt init` project bootstrap (#31); `rtrt project` lifecycle (status / health / repair) + team agents (#32).

### Highlights — Output Optimizer + Command Optimizer

- **Output Optimizer** (#10): compression as a persistent terse mode with a level (`off` / `lite` / `full` / `ultra`) instead of one-off compress calls; multilingual rules + auxiliary skills (#12); compressed-output crew subagents (#13); skills/agents load from user-level dirs with per-agent terse rules (#14); level toggle in the web UI (#15).
- **Command Optimizer** (#17): broad command-output filters (git / cargo / filesystem / search / HTTP / GitHub / containers / Kubernetes / Python / Go / Node / TypeScript / linters); `rtrt proxy-run <cmd>` executes a command and filters its output while preserving the exit code; transparent Claude Code `PreToolUse` hook (`rtrt hook proxy-rewrite`) rewrites shrinkable commands automatically; `rtrt gain` savings analytics from `~/.rtrt/proxy-stats.sqlite` (#20); `rtrt discover` scans Claude Code transcripts for shrinkable commands.
- Real-time per-project token savings across all three optimizers in the dashboard (#18); percent-reduction headline + Environment tab (#22).
- rtrt rebranded as **Retort** — "distills AI agent context" (#16).

### Fixed — since the June sweep

- Memory captures are attributed by git repo root, not cwd basename (#9).
- Dashboard: user-facing third-party agent names dropped from the statusline; defaults synced to agents (#41); Output Optimizer led by its level, not a misleading % (#28).
- CI: clippy 1.96 `sort_by_key` + Windows unused-path fixes (#35).
- Release pipeline: the crates.io publish loop in `release.yml` now covers all 11 workspace crates in dependency order and fails loudly on a real publish error (previously 9 crates were listed — omitting `rtrt-security` / `rtrt-eval` — and every failure was swallowed); an idempotent already-published skip is kept. `coverage.yml` enforces a line-coverage floor (`--fail-under-lines 35`); `cargo audit` in CI is blocking.

### Highlights — interactive memory graph (no-LLM similarity by default)

**The memory graph goes from scattered dots to an explorable map. The default mode needs no entity extraction and no generative LLM.**

- **Similarity mode (default)**: `graph_similarity` links each memory to its most-similar peers with weighted edges — cosine over already-stored embeddings (no inference call) or, when none exist, BM25 lexical overlap via FTS5 (fully model-free). `GET /api/memory/graph` returns `{ mode:"similarity", basis:"vector"|"bm25", nodes, edges:[{src,dst,weight}] }`. The UI defaults to this — the graph populates immediately, no extraction step.
- **Entity mode (opt-in, `mode=entity`)**: the bipartite memory↔entity graph (entities as first-class nodes), built by the LLM entity-extraction pass for users who want concept-level structure.
- UI: a similarity / entity mode toggle; similarity edges scaled by weight with a basis caption; the entity-extraction button lives only in entity mode.

- `rtrt-memory` schema v7 adds `entities(project, name)` + `memory_entities(memory_id, entity_id)` so extracted entities are first-class nodes (the old memory-to-memory `edges` path is kept, additive). New `upsert_entity`, `link_memory_entity`, `link_extracted_bipartite`, and `graph_bipartite` (returns memory nodes, entity nodes with degree, and memory→entity links, with each memory's `source_kind`).
- `GET /api/memory/graph` emits a bipartite `{nodes, edges}` (memory nodes `m<id>` with kind + source_kind, entity nodes `e<id>` with degree); `POST /api/memory/entities` now builds the bipartite graph.
- UI: entities render as large green nodes (radius by degree), memories as small nodes (blue main / purple subagent); force-directed layout with node drag/pin, wheel zoom, and pan; clicking a node opens a detail panel and highlights its neighbors; memory/entity + main/subagent filters and a search box; an empty graph shows a call-to-action to run entity extraction.

### Highlights — capture teammate/subagent work, grouped under the parent project

**The dashboard tails Claude Code's transcripts to capture teammate (FleetView) and subagent (Task-tool) work that never reaches the main agent's transcript, folds it under the real project, and classifies every row as main vs subagent.**

- New transcript watcher in `rtrt-dashboard`: tails `~/.claude/projects/**/*.jsonl` (main sessions + nested `<session>/subagents/agent-*.jsonl`), saving each assistant turn with `body_sha` dedup against existing hook captures.
- Subagent rows are attributed to the **parent session's project**, resolved from the parent transcript's cwd — stable even when the subagent ran in a git worktree (whose own cwd basename is a branch name like `p18-gap`, not the repo). Each row is tagged `source_kind = main | subagent`.
- `rtrt-memory`: `reattribute(id, source_kind, project?)` (one `json_set` UPDATE) + `reattribution_candidates()`; a boot-time migration folds pre-existing stray subagent/worktree buckets under their parent and classifies main/subagent (idempotent).
- `/api/projects` hides stray buckets (`agent-*` / `p<n>-*` / hex session hashes) from the project selector (registered projects always show); the selector dropped from 105 to the real project set.
- Timeline API exposes `source_kind`; the memory page shows 🧠 main / 🤖 subagent badges and an all / main / subagent filter.

### Highlights — project-centric dashboard + per-project security

**The dashboard is reorganized around a project context instead of a flat 13-item menu, and security becomes project-aware.**

- New project registry in `rtrt-core`: `ProjectEntry { name, path, security_profile }` + `Config.projects` with `project()` / `upsert_project()` helpers. Persisted to `~/.rtrt/config.toml` as `[[projects]]`.
- Dashboard API: `GET /api/projects` (config registry unioned with memory buckets → name / path / bound profile / mem_count), `PUT /api/projects` (upsert + config write-back), `POST /api/security/profile` (validate + save a custom profile to `~/.rtrt/security/profiles/`).
- UI: a global project selector in the sidebar (remembered via localStorage) with an add/edit-project modal; the nav is split into a **Project** scope group (Overview / Memory / Security / Code Map / Diagnose) and a **Tools** group (Compress / Local LLM / Prompts / Templates / Connections / Settings). Scope pages read one `currentProject()` instead of each carrying its own project picker.
- Security page is project-aware: it scans the selected project's path, defaults to that project's bound profile, an "Apply to this project" control binds a profile to the project, and a "Profile settings" subtab lists / views (rules + standards) / clones / saves profiles.

### Highlights — security & license profiles for AI-generated artifacts

**New `rtrt-security` crate (11th workspace crate): profile-driven security + license scanning modeled on RHEL/OpenSCAP profiles. Six built-in profiles map every rule to industry standards (CWE / OWASP Top 10 + ASVS / NIST 800-53 + 800-218 SSDF / CIS Controls v8 / SLSA / EU AI Act), run by five pluggable engines, surfaced through the CLI, the dashboard, and MCP.**

- Five scan engines: `secrets` (builtin pattern set + Shannon-entropy gate + redacted excerpts), `licenses` (SPDX manifest policy, allow/forbid lists, optional header check, workspace-inheritance aware), `deps` (Cargo.lock / package-lock hygiene — git/wildcard/yanked — plus optional offline RustSec advisory match), `patterns` (regex source scanner with lang + path filters), and `ai` (AI-artifact-specific: hallucinated-import / slopsquatting, base64-blob, eval-usage, todo-secret, unsafe-block — each source file judged against its nearest crate manifest so monorepo members don't false-positive).
- Six built-in profiles: `ai-default` (recommended baseline, 10 rules), `ai-strict` (16), `owasp-top-10` (15), `asvs-l2` (13), `cis-baseline` (13), `nist-ssdf` (12). Declarative TOML — users drop their own under `~/.rtrt/security/profiles/` to override built-ins or add new ones. Every rule carries a `standards` mapping so findings cite the control they enforce.
- CLI: `rtrt security scan --profile <name> [--path] [--json]`, `profile list`, `profile show <name>`, `gate` (non-zero exit at/above the profile threshold — CI gate), `init` (copy built-ins to the user dir for customizing).
- Dashboard: a Security page (profile picker, scan, severity-grouped findings with standards chips, engines run/skipped) backed by `GET /api/security/profiles`, `GET /api/security/profile/{name}`, `POST /api/security/scan`.
- MCP: `security_scan(profile, path?)` returns the full ScanReport so an agent can self-check its own output before committing.

### Highlights — dashboard auto-starts as a background service

**The installer now runs `rtrt-dashboard` as a background OS service, so the web UI at <http://127.0.0.1:7311> is always up without launching it by hand — it restarts on crash and comes back on login.**

- New `rtrt service install|uninstall|status` subcommand: a systemd **user** unit on Linux (`~/.config/systemd/user/rtrt-dashboard.service`) and a launchd LaunchAgent on macOS (`~/Library/LaunchAgents/io.kodenet.rtrt-dashboard.plist`). Dry-run by default; pass `--apply`. The unit pins `RTRT_MEMORY_PATH=~/.rtrt/memory.sqlite` so the service reads the same store as the CLI/MCP/hooks.
- `install.sh` / `install.ps1` start the service by default (Windows registers a `rtrt-dashboard` logon scheduled task); opt out with `--no-service` / `-NoService` / `RTRT_NO_SERVICE=1`. `uninstall.sh` / `uninstall.ps1` stop + remove it before deleting binaries.
- `docs/INSTALL.md` + `docs/INSTALL.ko.md` document the service, the opt-out flag, and manual management.

### Highlights — consistent savings %, local LLM management page

**The dashboard now reports compression savings as a percentage everywhere, and a new page manages local Ollama models end to end.**

- Every compression surface returns `saved_pct` (1-decimal): `POST /api/compress`, `POST /api/proxy`, `POST /api/memory/compress`, plus `GET /api/memory/stats` (`saved_pct` aggregate over compressed rows) and the timeline rows (per-row `saved_pct`, null when uncompressed). The UI shows the percentage on the compress/proxy result, a stats KPI tile, and per-row badges.
- New **Local LLM** dashboard page backed by `GET /api/ollama/models`, `GET /api/ollama/ps`, `POST /api/ollama/pull`, and `DELETE /api/ollama/models`: list installed models with size, see currently-loaded models, pull a new model (blocking), delete a model (with confirm), and set any model as the compression or embedding default in one click. Ollama base URL resolves from config (`embeddings` → `auto_compress` → localhost, trailing `/v1` stripped).

### Highlights — dense-vector semantic recall, entity linking, SessionStart injection

**Memory recall gains a true dense-vector path backed by a local Ollama embedder, the dashboard exposes one-click embedding backfill + entity extraction, and a SessionStart hook injects project knowledge from turn one.**

- `rtrt-memory::OllamaEmbedder` (behind the `ollama-embed` feature) calls `{base_url}/api/embeddings`; the default model is `bge-m3` (1024-dim). Wired into `recall_hybrid` so `mode=hybrid` recall fuses BM25 + cosine via reciprocal-rank fusion when an embedder is present, and degrades to graph-blended BM25 when it is not.
- New `[embeddings]` config section (`enabled` / `model` / `base_url`) with `RTRT_EMBED_ENABLED` / `RTRT_EMBED_MODEL` / `RTRT_EMBED_BASE_URL` env overrides; surfaced read+write through `GET`/`POST /api/config` and the dashboard settings page.
- Dashboard: `POST /api/memory/embed` backfills embeddings for a project's un-embedded rows; `POST /api/memory/entities` runs LLM entity extraction and links co-mentioning memories. The search subtab shows a semantic badge on vector hits and an embed-backfill button; the graph subtab adds an entity-extract button with typed (entity / block / memory) node colouring.
- `MemoryStore::add_edge` now returns whether a new edge was created; entity linking is split into the async `link_entities` and a synchronous `link_extracted` so the work runs from a `Send` axum handler without holding a `!Sync` store borrow across an `.await`.
- New `rtrt hook session-inject` (registered on `SessionStart`) prints the project's top memories as a context block so background knowledge is available before the first prompt.

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
- `install.sh` + `install.ps1` one-liners wired to GitHub Releases with SHA256 verification; `release.yml` builds 5 targets (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`), attaches them to the GitHub Release, and publishes every workspace crate to crates.io on a `REL-vX.Y.Z` marker tag.
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

- **Headroom-aware routing everywhere**: MCP `agent_route` and the dashboard route preview/`/api/route` now rank candidates on the same ledger-overlaid usage snapshot as the CLI (`UsageSnapshot::load_for_routing()`, new shared helper), instead of routing blind on stale best-effort data.
- **MCP detection parity**: `agent_route` detects targets with the effective (global ⊕ project `.rtrt/config.toml`) config via `detect_tools_with_config`, so per-project agent/provider enable maps apply to MCP routing; `agent_route`'s capability parser accepts `agentic`.
- **Configurable API answer length**: the routed API-mode output-token ceiling is no longer a hardcoded 1024 — it resolves `RTRT_API_MAX_TOKENS` env → `[providers] api_max_tokens` (global or per-project) → a 4096 default. New `rtrt_core::repo_root_from` / `Config::load_effective_for_cwd()` helpers back the per-project resolution.
- `rtrt-core`: `CompressionLevel` and `Config` switch to `#[derive(Default)]` with `#[default]` enum variant; manual impls removed (clippy `derivable_impls`).
- `rtrt-providers` workspace deps add `eventsource-stream`, `futures-util`, `mockito`.
- `Cargo.toml` adds workspace deps for `rmcp`, `schemars`, `criterion`, `fastembed`, `eventsource-stream`, `futures-util`, `mockito`, `tree-sitter`, `tree-sitter-rust`, `instant-distance`, `handlebars`.
- `rtrt-memory` schema gains a `scope` column on `memories` via a `PRAGMA user_version`-gated `ALTER TABLE` migration. Existing databases pick up the column with default `'project'` on first open.

### Fixed

- **Router telemetry split-brain**: only the CLI recorded provider usage; MCP `provider_chat`, the dashboard chat handler, and every other gateway consumer were invisible to the headroom ledger. `Gateway::from_env` gateways now append each dispatched request to the provider-usage ledger (real API counts as exact `est=0` rows, chars/4 estimates with `est=1` when no usage block is returned, failures as `ok=0`); `Gateway::new` stays ledger-silent for embedded/test use, and the `rtrt route`/`call` API path opts out of gateway recording to keep its tool-name-attributed rows without double counting.
- **Ledger trim race**: `trim_to_cap`'s read-rewrite could drop concurrently appended rows when multiple rtrt processes wrote the ledger at once. Writers now serialize behind a best-effort `provider-usage.tsv.lock` (`O_EXCL` create, bounded retries, 10s stale-steal); when the lock is contended the append still lands and only the trim is skipped.
- `rtrt-compress`: the protection layer now stashes bare path-shaped (`docs/reference/api.md`) and filename/version-shaped (`main.rs`, `1.2.3`) tokens, so abbreviation and article rules can no longer corrupt identifiers outside backticks (`docs/reference/api.md` no longer becomes `docs/ref/api.md`).
- `rtrt-compress`: pleasantry removal (`sure`, `let me`, …) is anchored to sentence starts — "Make sure you do not delete" keeps its "sure" instead of becoming "Make you do not delete".
- `rtrt-compress`: word deduplication skips numeric tokens, so repeated data points ("10 10 10") are no longer collapsed to one.
- `rtrt-compress`: the heuristic ML scorer hard-keeps negations (not / don't / never / without / unless / …), numerals, and error-code-shaped tokens (E0308, HTTP 500) — "do not delete the production database" at ratio 0.5 no longer compresses to "delete production database".
- `rtrt-compress`: when whatlang misclassifies short technical English as another language, mostly-ASCII text (>=90%) still gets the English rule set instead of silently skipping all compression.
- `rtrt-proxy`: the `gh` filter had its polarity inverted — it dropped `X `-prefixed FAILING check lines and kept `✓` pass lines. Failures are now always kept verbatim and pass lines collapse into a single `✓ N passed` summary.
- `rtrt-memory`: punctuated natural-language recall queries (`don't`, `foo-bar`, `C++ (auth)`) no longer hard-fail with FTS5 errors ("fts5: syntax error" / "no such column"). `recall_bm25` / `recall_bm25_scoped` (and everything layered on them — hybrid, filtered, graph-blend recall) try the query verbatim first, then fall back to a shared `sanitize_fts_query` OR-join; a query with no usable term returns zero hits instead of an error. The `hook recall` prompt sanitizer now reuses the same shared function.
- `rtrt-memory` / `rtrt-mcp`: `memory_consolidate` no longer silently deletes old rows. `archive_overflow_no_llm` writes an archival digest row first (kind `archival`, payload `archive=true`, one preview line per archived row, √n-scaled preview budget) and never re-consumes digests, and the MCP tool description now says exactly what happens (no LLM summarisation) and returns the digest row id.
- Default memory store path unified across every surface via `rtrt_core::default_memory_store_path()` (`~/.rtrt/memory.sqlite`): CLI `memory save/recall/export/import/extract/compress/blocks`, `rtrt mcp`, the `rtrt-mcp` server, and the dashboard previously defaulted to a cwd-relative `.rtrt/memory.sqlite`, so a fresh install's recall silently queried a different, empty database than the one the hooks wrote to. Explicit `--store` / `--memory` / `RTRT_MEMORY_PATH` overrides keep working.
- `rtrt-memory`: graph traversal (`memory_relations`) is bounded and project-scoped during the walk — the BFS visit budget scales as `√rows` with a floor instead of being unbounded, and rows from other projects can no longer act as traversal bridges (previously the project filter ran only after the full walk).
- AIPS plugin workaround at init time: `lib/detect-project.sh` emits unquoted multi-word values (e.g. `DEPLOYMENT=GitHub Actions`), which breaks `lib/render-claude-md.sh`'s `eval` call. Worked around locally.
- `rtrt-cli` clippy fixes on stable: `sort_by(|a,b| b.cmp(a))` → `sort_by_key(Reverse(...))`; manual `if zero { 0 } else { x*100/y }` → `checked_sub` + `checked_mul` + `checked_div` chain.
- Install/uninstall hardening: uninstallers now unwire the Claude Code integration (MCP + hooks + statusline via `rtrt uninstall --agent claude --plugin --apply`) and fall back to direct service-unit cleanup; `rtrt uninstall --agent claude` drops the rtrt `statusLine` entry; `install.sh` gains pipefail (where supported), a fixed `--ref` clone fallback, working release-path `--dry-run`, and a hard error when a published SHA256 can't be verified; `install.ps1` forces TLS 1.2+ and safe PATH guidance (no `setx` truncation); docs drop the broken `irm | iex -Args` pattern.

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
