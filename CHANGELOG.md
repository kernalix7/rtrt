# Changelog

**English** | [한국어](docs/CHANGELOG.ko.md)

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-10-03

Addresses the v0.2.0 audit. Source builds require Rust 1.88; CI verifies that compiler floor.

### Fixed

- Ordinary npm and Unix CLI browser launchers receive an owner-private `bootstrap.html` path instead of a capability URL. `rtrt service open --print-bootstrap` remains explicit; 60-second HMAC, Origin, expiry, and replay controls are unchanged.
- Windows credential/state ACLs remain current-SID-only, with unsafe existing state refused rather than rewritten. Binary trust recognizes TrustedInstaller, expands generic rights, and rejects untrusted effective file writes, delete-child, ACL, and ownership rights. Sixteen policy regressions are Windows-only.
- A missing projects root is empty state, not a phantom unsafe project; unsafe paths remain refused.
- Offline Cargo sandbox builds extract cached crates into private writable sources while host cache/index stay read-only.

### Distribution

- Conservative notice coverage maps 265 registry components across five targets and three products: 470 original archive paths, including eight metadata manifests, plus nine full-text supplements for eight crates. Both `matchit` MIT AND BSD-3-Clause texts and the unchanged MPL `option-ext@0.2.0` source archive are included. This is not an optimized SBOM or legal certification.
- Publishing rejects existing-byte/paired-tag conflicts, verifies completed drafts before publication, and never overwrites assets. Archive READMEs link to version-pinned documentation. All six published 0.2.0 npm packages and old tags remain unchanged.
- Native TUI regressions configure `cli.json` independently from server options; v1 contracts remain unchanged.

### Notes

- Actual Windows/system-drive acceptance is required before publication; Linux skips are not Windows proof. No Docker, collector, crates.io, or Homebrew tap activation is included.

## [0.2.0] - 2026-10-01

### Highlights

**RTRT 0.2.0 adds native OpenCode 2.0.20 integration alongside the existing v1 integration.**

- The npm agent includes a native `./server` plugin definition and a `./tui` prompt-footer statusline. The v1 named `RtrtProvenance` root export and legacy setup-managed statusline remain separate and compatible.
- Native hooks preserve RTRT tool provenance, dashboard lifecycle, explicit dashboard opening, and permission denials. External Claude CLI permission brokerage, per-call shell identity, and provider-limit recovery remain unsupported because the 2.0.20 public API does not expose those surfaces.

### Fixed

- Dashboard `/api/*` Origin validation now rejects present malformed or repeated headers instead of treating them as absent. Deliberately absent-Origin bearer clients remain supported, and bootstrap still requires an allowed Origin.
- Published npm staging omits repository-only development scripts while preserving runtime files, executable permissions, exports, and exact-version platform dependencies.

### Distribution

- Distributions include the complete, unchanged `option-ext@0.2.0` MPL-covered source archive with a pinned SHA-256 and local extraction instructions. The existing targeted license notices remain intact; this is not a legal certification or an exhaustive dependency inventory.
- Existing `0.1.7` tags and published packages remain unchanged. This release does not publish Docker images or activate an operational collector, sandbox, or global configuration.

## [0.1.7] - 2026-09-28

### Highlights

**RTRT 0.1.7 prepares a separate release after the partial v0.1.6 publication, with a longer bounded wait for npm registry visibility.**

- After `npm publish`, platform verification now waits for up to 60 registry checks at 10-second intervals and accepts only a matching package name, version, and SHA-512 SRI. A mismatch still fails closed; a timed-out publish must not be followed by a blind rebuild of an already accepted version.
- The `rtrt-agent` visibility check uses the same bounded window. A `main`-dispatched `REL-` publication recovery is refused before building because the protected `npm-publish` environment allows tag refs only.

## [0.1.6] - 2026-09-28

### Highlights

**RTRT 0.1.6 addresses the v0.1.5 audit findings with pair-scoped collector credentials, bundled third-party notices, mobile project selection, and broader CI coverage.**

- `rtrt collector serve` now binds each `(guest_id, project)` mapping to its own bearer credential through a per-map `--credentials` file, and the `forward flush` path has a matching `--guest-id` / `--project` filter so one token can no longer write to an unrelated host store. The single `--token` form is retained only when exactly one mapping is configured.
- The dashboard's mobile layout keeps the project picker visible at `≤720px` instead of hiding the entire sidebar, and wide overview content scrolls within its card rather than widening the page.
- A new CI job exercises the optional feature surfaces (`embeddings`, `onnx`, `bertscore`, `chains`) at `cargo check --locked` depth so a future build-time regression in any of them is caught even when no model download is permitted. A `macos-15-intel` runner is added to the test matrix.
- A new `THIRD_PARTY_NOTICES/` directory ships the identified missing MIT notices for embedded dashboard bundles and pinned texts for selected Rust dependencies. `THIRD_PARTY_LICENSES.md` now matches their resolved versions; legal completeness is not certified.
- Live-key, WSL, and macOS x64 coverage is documented honestly: live provider keys remain a pre-tag-only `scripts/smoke.sh` step, WSL has no dedicated CI lane, and `macos-15-intel` joins the matrix in this release rather than relying on a `macos-latest` arm64-only run.

### Added

- `rtrt collector serve` accepts a `--credentials <TOML>` file that pairs one bearer token with one `(guest_id, project)` mapping, and refuses a shared bearer across mappings. The file must be operator-owned with mode `0600`; path components are checked for symlinks, and the opened file's identity, owner, and mode are checked again. On non-Unix targets, credential files are refused because the ownership and mode checks have no portable meaning. The `--token` form remains available for one mapping; the two auth sources cannot be combined.
- The authorization middleware resolves the constant-time bearer against every binding and injects `AuthorizedBinding { pair, identity }` without the token into the request extension. The ingest handler refuses events whose `guest_id` or `project` differs from that binding, so one pair's token cannot authorize another mapping. A binary integration test confirms that A's token receives `403` for B without creating B's SQLite store.
- `rtrt forward flush --guest-id <id> --project <name>` filters the spool to a single pair before delivery so a guest can drain only its own queue. The new `Selection` enum replaces the prior `pending(limit, force)` helper; filtered selection scans the full rowid stream but limits the deliverable batch to 100 due rows so a foreign row cannot starve a matching one, and the spool-side validation still fails closed on malformed or mismatched payloads without mutating the queue. Unfiltered flush still rejects mixed-pair spools because no single token could authorize delivery to both host stores.
- `THIRD_PARTY_NOTICES/INDEX.md` records versioned source archives and SHA-256 digests for the identified bundled JavaScript and selected Rust dependency license texts, including `ring`, `subtle`, `webpki-roots`, and `option-ext`. The five binary archives, five platform npm packages, and `rtrt-agent` include the same notice tree. This is a targeted inventory, not a legal certification or a substitute for reviewing MPL-2.0 source availability.

### Changed

- `MemoryStore::ingest_forwarded` and the wire shape (`WireEvent` / `ForwardedEvent`) are unchanged. The collector's authorization now binds a token to a specific `(guest_id, project)` pair before the ingest handler ever reads the body, so the existing idempotency guarantee on `(source_guest, source_project, event_id)` is reinforced rather than replaced.
- `THIRD_PARTY_LICENSES.md` now records the resolved versions accurately: `ring@0.17.14` is described as ISC/Apache-2.0 with the bundled BoringSSL split, `subtle@2.6.1` is BSD-3-Clause, `webpki-roots@0.26.11` and `webpki-roots@1.0.7` are CDLA-Permissive-2.0, and `option-ext@0.2.0` is MPL-2.0. The mislabeled MPL-2.0 entry for webpki-roots and the stale ring license mix from the v0.1.5 record are removed.
- The dashboard's mobile layout keeps the project picker in view: the sidebar's `mode-nav` group hides at `≤720px` while the `project-picker` row stays visible, the main column min-width drops to `minmax(0, 1fr)` to stop long slugs from forcing a horizontal scrollbar, and the overview card plus savings hero get `overflow-wrap: anywhere` so long cells wrap instead of clip.
- Paired install docs show the v0.1.6 pin and describe the release archive names. The in-source Homebrew formula remains a placeholder until its checksum is replaced and a separate tap change is published.

### Fixed

- The collector shared-token impersonation path demonstrated in the v0.1.5 audit is closed in the built CLI integration path. `crates/rtrt-cli/tests/collector_auth.rs` starts the actual collector binary with two projects and distinct tokens, checks that A's token receives `403` for B without creating B's store, and verifies that flushing A's events leaves B's queued rows unchanged.
- The dashboard no longer hides the only project selector below `720px`. A `.project-picker` rule removes its bottom border on small viewports and the `.savings-hero` plus overview card constrain long tokens; mobile sessions can now change the selected project without resizing the window.
- The OpenCode CI plugin's `npm test` job now includes `notices-package.test.mjs`, which asserts that every entry in `THIRD_PARTY_NOTICES/INDEX.md` is present in the packed `rtrt-agent` tarball and that each `LICENSE` matches the INDEX-recorded SHA-256, so a future bump that drops or edits a notice fails CI before publish.

### CI

- A new `feature-lanes` job runs `cargo check --locked -p rtrt-memory --features embeddings` with `ORT_SKIP_DOWNLOAD=1`, `cargo check --locked -p rtrt-compress --features onnx`, `cargo check --locked -p rtrt-eval --features bertscore`, and `cargo check --locked -p rtrt-templates --features chains`. Each is compile-only so the lane never needs an ONNX model download or a live provider call. This does not claim that every optional feature compiles into every default binary — the v0.1.5 audit record already corrected that blanket assertion — it adds one bounded compile lane per feature so a future regression is caught.
- The test matrix gains a `macos-15-intel` runner entry alongside the existing `ubuntu-latest` x64/arm64, `macos-latest` arm64, `windows-latest`, and `beta`-toolchain Linux x64 lanes. macOS x64 was previously built but not tested in CI; it is now part of the matrix rather than relying on the arm64 run.
- Live Anthropic / OpenAI / OpenAI-compatible checks remain a `scripts/smoke.sh` pre-tag gate, not a CI lane, because the CI environment does not hold provider keys; the WSL runtime also has no dedicated CI lane because the WSL image does not fit cleanly into the existing runner labels. Both are documented in the audit follow-up rather than papered over.

### Notes

- Live-provider smoke requires user-supplied keys and is not a CI lane; WSL has no dedicated runtime job. The added optional-feature jobs compile without model downloads, while macOS Intel is now tested in CI.
- The Homebrew formula retains an all-zero SHA-256 placeholder and is not an installable tap release. The notice tree does not certify legal compliance; notice placement and MPL-2.0 source-availability obligations remain for qualified counsel review.

### Publication outcome (factual addendum)

- The paired tags `v0.1.6` and `REL-v0.1.6` both point at commit `31bdd0f` and remain on `origin`; they were not moved and the SHA is unchanged.
- GitHub Actions run `36374328637` attempt 2 failed on the immutable Windows SRI calculation; the `main`-branch `workflow_dispatch` recovery run `36383130874` was rejected by the `npm-publish` environment's tag-only policy.
- All five `rtrt-dashboard-<platform>` npm package versions at `0.1.6` were published successfully (`200` from the npm registry for each). `rtrt-agent@0.1.6` and the GitHub Release for `v0.1.6` were never published.
- The partial state cannot be repaired by moving the immutable tags or by republishing the existing `0.1.6` platform package versions. The five `rtrt-dashboard-*@0.1.6` packages are not a completed release; `rtrt-agent@0.1.6` does not exist on the npm registry.

## [0.1.5] - 2026-09-26

### Highlights

**RTRT 0.1.5 lets containerized and remote guests write explicit memory events into host-owned projects through a one-way, bearer-authenticated ingest loop: the host runs `rtrt collector serve` with an explicit `(guest_id, project)` map, the guest enqueues into a private durable spool with `rtrt forward enqueue`, and `rtrt forward flush` retries with bounded exponential backoff until the host collector stores the event exactly once.**

- `rtrt collector serve` exposes an authenticated `POST /v1/events` endpoint that consolidates container or remote-guest memory events into host projects. The collector refuses every browser `Origin` request without reading the request body, refuses missing and wrong bearer tokens without reading the request body, caps each request body at 1 MiB, and derives every destination from a host-controlled project path so a guest cannot pick its own target.
- `rtrt forward enqueue|flush` writes every event into a private durable SQLite spool before any delivery attempt. The spool survives crashes, retries with bounded exponential backoff, and only drops an event when the host collector acknowledges it, so retries never create duplicate host memory rows.
- The host collector stores each `(guest, project, event id)` delivery exactly once through the new `MemoryStore::ingest_forwarded` idempotent forward-ingest path, which lives on schema v9; redeliveries of an already-received tuple are acknowledged without writing again. The same tuple in another host project is a distinct delivery and inserts there.
- The guest spool's path validation and Unix ownership guards apply across supported Unix targets; the forward spool also works on Windows without Unix-specific owner and mode checks. On Unix, the immediate spool directory must be operator-owned with mode `0700` and the spool file with mode `0600`. Symlinked path components are refused before anything is written through them.
- CI now stages OpenCode test dependencies under `.rtrt/tmp/npm-test-dependencies` before running `npm ci`, so optional platform packages and other uncontrolled transitive deps no longer fail the OpenCode plugin's lockfile-gated test job.

### Added

- `rtrt collector serve` is an authenticated `POST /v1/events` ingestion endpoint for consolidating container or remote-guest memory events into explicitly mapped host projects. The authorization middleware refuses every `Origin` request, missing bearer, and wrong bearer with `FORBIDDEN` / `UNAUTHORIZED` and never buffers the body when it does so; each accepted request is then capped at 1 MiB and parsed as JSON. Every destination is derived from a host-controlled project path so guest-supplied paths are never accepted, and retries are deduplicated by stable event id. The server defaults to `127.0.0.1:7313` and requires `RTRT_COLLECTOR_TOKEN` on every request. Bind a private bridge address and terminate TLS in a reverse proxy when traffic leaves the host-local network; the collector does not implement TLS.
- `rtrt forward enqueue` queues an explicit memory event into `~/.rtrt/forward-spool.sqlite` and `rtrt forward flush` retries due events with bounded exponential backoff. Each event carries a stable id; delivery uses bearer-authenticated POST and only removes the queued row after a valid collector acknowledgement. Failed deliveries remain queued and can be retried without creating duplicate host memory rows.

### Changed

- `MemoryStore::ingest_forwarded` is now the host's only forward-ingest path. It keys idempotency on `(source_guest, source_project, event_id)` and commits the memory row, its FTS5 mirror, the merged metadata (caller keys plus `source_guest` / `source_project` provenance), `session_id` / `body_sha`, and the `forwarded_event_receipts` row in a single SQLite transaction so a crash can never leave a memory without its receipt (which would let a redelivery duplicate it) or a receipt without its memory. A redelivery returns the original `memory_id` with `inserted = false`; a fresh tuple with the same `event_id` from a different guest or remote project is treated as a distinct delivery and inserts in its own row. The wire shape (`WireEvent` / `ForwardedEvent`) is unchanged.

### Fixed

- The guest forward spool applies the path-validation and operator-owned guards uniformly on every supported Unix platform. Ownership checks read the effective UID through `/proc/self/status` on Linux and Android and through `id -u` on other Unix platforms (the Unix ownership and mode checks themselves do not apply on Windows, where the spool uses a non-Unix path). The immediate spool directory must be operator-owned with mode `0700`, the spool file must be operator-owned with mode `0600`, and every symlinked path component — parents, intermediate directories, the file path itself — is refused. If the immediate spool parent directory does not yet exist, `open_spool()` calls `private_parent()` first, which can create the missing immediate parent directory with mode `0700`; only after that step does the leaf-file symlink check run. No symlinked component is ever populated by this code.

### CI

- OpenCode plugin CI stages locked test dependencies under `.rtrt/tmp/npm-test-dependencies` before running `npm ci --ignore-scripts`, so optional platform packages and other uncontrolled transitive dependencies no longer break the OpenCode plugin's lockfile-gated test job. The staged `node_modules` is then moved into the test working directory; existing fixture behavior is unchanged.

## [0.1.4] - 2026-09-21

### Highlights

**RTRT 0.1.4 closes the loop on OpenCode session migration: `rtrt opencode sessions backup` snapshots every session store this user owns without carrying secrets, and `--source` restores any snapshot through the existing migration path.**

- `rtrt opencode sessions backup` writes `global.sqlite`, `projects/<slug>.sqlite`, and a content-free `manifest.json` into one private `0700` root, and refuses existing, traversing, or symlinked output paths.
- Snapshots use SQLite's online backup API from a read-only handle, so the live store is never modified and in-flight WAL content is captured as one committed snapshot.
- Credential-bearing tables keep their schema and lose every row; copied triggers and views fail closed before scrubbing, and each snapshot is detached from WAL and vacuumed so no freed secret pages remain.
- `status`, `dry-run`, and `apply` accept `--source <db>`, so session migration is no longer one-way.

### Added

- `rtrt opencode sessions backup` snapshots the global OpenCode store and every RTRT-private project store into one private backup root (`global.sqlite`, `projects/<slug>.sqlite`, `manifest.json`). Snapshots are taken with SQLite's online backup API from a read-only handle, so WAL content is captured as one committed snapshot and the live store is never modified. Sensitive tables keep their schema and lose every row, matching the exclusion set migration already enforces, and each snapshot is detached from WAL and vacuumed so no freed credential pages remain. The root is mode `0700`, files are `0600`, an existing path is refused instead of merged, and the manifest is written last as the completeness marker.
- `rtrt opencode sessions status|dry-run|apply` accept `--source <db>`, so a backup can be restored through the existing migration path and session migration is no longer one-way.

### Fixed

- Documented session migration accurately: it never modifies the source database, rather than retaining it "as backup".

## [0.1.3] - 2026-09-16

### Highlights

**RTRT 0.1.3 makes the native dashboard available automatically from `rtrt-agent` while keeping browser launch explicit, local, and credential-safe.**

- OpenCode plugin load now starts one version-matched, loopback-only dashboard backend without requiring `PATH`, a preinstalled RTRT binary, or install scripts.
- Native dashboard executables ship through five exact-version npm platform packages, with the release workflow publishing and verifying them before `rtrt-agent`.
- Existing `~/.rtrt` data and valid private credentials are preserved; unsafe ownership, modes, symlinks, and foreign listeners are refused.

### Added

- `rtrt-agent` now schedules the version-matched `rtrt-dashboard` backend as a detached, loopback-only per-user process when OpenCode loads the plugin. Five exact-version optional npm packages provide the native executables without install scripts, `PATH`, or a preinstalled RTRT binary.
- Browser launch is explicit through `rtrt-dashboard-open` or the argument-free `rtrt_dashboard_open` tool. Both use the existing 60-second HMAC bootstrap fragment and never place credentials in prompts or command templates.

### Changed

- Dashboard startup is fail-soft, reuses a healthy singleton, preserves valid private credentials and all existing `~/.rtrt` data, and refuses unsafe ownership, modes, symlinks, or foreign listeners.
- The paired-tag release workflow publishes and verifies all five dashboard platform packages before publishing `rtrt-agent`; the GitHub Release remains last.

### Fixed

- Updated `rustls` to `0.23.45` to address `RUSTSEC-2026-0285` and replaced the yanked `chacha20 0.10.0` lock entry with `0.10.2`.

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
