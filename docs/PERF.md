# Performance

**English** | [한국어](PERF.ko.md)

> "Performance is measured, not claimed." — [`DESIGN.md`](../DESIGN.md#6-performance-is-measured-not-claimed)

This page publishes the SLOs we commit to and the most recent measured numbers. Regressions beyond 10% block a release.

## Service Level Objectives

### CLI / library operations

| Operation | Input | p50 target | p99 target | Notes |
|-----------|-------|-----------|-----------|-------|
| `rtrt --help` | — | < 10 ms | < 20 ms | Cold start |
| `rtrt compress -l ultra` | 4 KB text | < 0.5 ms | < 1 ms | Rule engine |
| `rtrt compress --ml --ratio 0.5` | 4 KB text | < 1 ms | < 3 ms | Heuristic scorer (no ONNX) |
| `rtrt memory save` | 1 KB body | < 2 ms | < 5 ms | SQLite WAL |
| `rtrt memory recall` (BM25) | 1 K rows | < 5 ms | < 15 ms | FTS5 |
| `rtrt memory recall` (BM25) | 100 K rows | < 50 ms | < 150 ms | |
| `rtrt memory recall` (hybrid + HNSW) | 100 K rows | < 100 ms | < 250 ms | embeddings + hnsw features |
| `rtrt signatures --lang rust` | 8 KB file | < 5 ms | < 15 ms | tree-sitter |
| `rtrt repo-map` | 1 K Rust files | < 3 s | < 8 s | I/O bound |
| `rtrt-mcp` stdio handshake | — | < 30 ms | < 80 ms | |
| `rtrt-dashboard` first paint | localhost | < 50 ms | < 120 ms | Inline HTML, no JS bundle fetch |

### Auto-capture pipeline

The on-write path (dedup + privacy + save + tag) must stay light so the calling agent never feels it.

| Step | p99 target |
|------|-----------|
| Dedup window lookup | < 0.1 ms |
| Privacy filter (`redact_secrets`, 4 KB) | < 0.5 ms |
| SHA-256 (4 KB) | < 0.1 ms |
| SQLite save | < 5 ms |
| **End-to-end auto-capture** | **< 6 ms** |

The optional LLM-compress step runs in a background tokio task; the response path always returns after step 3.

### Resource caps

| Binary | Idle RSS | Peak RSS |
|--------|----------|----------|
| `rtrt` (most subcommands) | < 10 MB | < 50 MB |
| `rtrt-mcp` (idle) | < 15 MB | < 80 MB |
| `rtrt-dashboard` (idle) | < 20 MB | < 100 MB |

### Compression / recall quality (long-term targets)

Quality metrics require labelled datasets. The opt-in `rtrt-eval` crate ships a hand-tuned smoke fixture (`crates/rtrt-eval/fixtures/recall_smoke.json`) and accepts external fixtures with the same shape — drop in LongMemEval-S, Memorybench, or an in-house corpus to get the real numbers.

| Task | Metric | Target |
|------|--------|--------|
| `compress` meaning preservation | BERTScore F1 vs source | > 0.85 (full level) |
| `compress` token savings | mean char reduction | > 35% (full), > 55% (ultra) |
| `memory recall` (BM25) | R@5 on LongMemEval-S | > 0.80 |
| `memory recall` (hybrid) | R@5 on LongMemEval-S | > 0.92 |
| `memory recall` (hybrid) | MRR on LongMemEval-S | > 0.78 |

## Last measured

These tables are filled in from the criterion suite at the noted commit. Run `cargo bench --workspace` to refresh them locally.

### `rtrt-memory` recall benchmark — 2026-05-21

Hardware: laptop, Rust 1.85 stable, release profile, in-memory SQLite.

| Bench | Size | p50 | Within SLO? |
|-------|------|-----|-------------|
| `recall_bm25` | 1 K rows | **32 µs** | ✅ (target 5 ms) |
| `recall_bm25` | 10 K rows | **69 µs** | ✅ (target 50 ms) |
| `recall_bm25` | 100 K rows | **443 µs** | ✅ (target 50 ms) |
| `recent_paged` (limit=50) | 1 K rows | **29 µs** | ✅ (post-v5 index) |
| `recent_paged` (limit=50) | 10 K rows | **30 µs** | ✅ (post-v5 index) |
| `recent_paged` (limit=50) | 100 K rows | **32 µs** | ✅ (post-v5 index, was 71 ms) |
| `save_one` | 1 K rows | **25 µs** | ✅ (target 2 ms) |
| `save_one` | 10 K rows | **26 µs** | ✅ |
| `projects_listing` | 8 projects × 1 K | **629 µs** | ✅ |

**Notes**

- `recall_bm25` stays under the SLO at every size — FTS5 is doing its job.
- `recent_paged` was the obvious miss at 100 K (71 ms). Schema v5 adds a covering index on `(project, created_at DESC, id DESC)` and the query now serves off a single seek + sequential walk; p50 dropped to ~32 µs across all sizes (2200× faster on the 100 K bucket).
- `save_one` is constant — the WAL journal absorbs writes.

### `rtrt-compress` benchmark — last published

See `crates/rtrt-compress/benches/compress_bench.rs`. The README's "60%+ savings" claim is measured here per fixture × level. Refresh with `rtrt benchmark`.

### `rtrt-eval` smoke fixture — 2026-05-22

Hardware: laptop, Rust 1.85 stable, debug profile, in-memory SQLite. Refresh with `cargo run -p rtrt-eval -- recall` / `compress`.

| Surface | Metric | Value |
|---------|--------|-------|
| `recall_bm25` (built-in `recall_smoke`, 12 docs, 7 queries) | R@5 | **0.857** |
| `recall_bm25` (same fixture) | MRR | **0.857** |
| `compress lite` (built-in `compress_smoke`) | mean ratio | **0.962** |
| `compress full` | mean ratio | **0.932** |
| `compress ultra` | mean ratio | **0.879** |

The R@5 floor of 0.80 is enforced by `rtrt_eval::tests::recall_at_5_on_smoke_fixture_clears_floor`. The smoke fixture is intentionally tiny — replace it with a real labelled corpus to publish trustworthy numbers.

### LLM auto-compress — local model sweep — 2026-05-26

Char reduction of the SessionEnd / dashboard LLM compress path, measured against an Ollama backend over 20 realistic captures per length tier (commands, logs, stack traces, prose, diffs, decisions). `skip` = rows left unchanged because the model produced no shrink (the `compressed_skip=no-shrink` guard). Reduction is `1 - out_chars/in_chars`.

| Tier (chars) | gemma3:4b | gemma3:12b | granite4.1:8b |
|--------------|-----------|------------|---------------|
| XS (~16)     | 2.8% (18 skip) | 8.6% (15 skip) | 1.2% (19 skip) |
| S  (~90)     | 9.0% (8 skip)  | 31.3%          | 8.2% (8 skip)  |
| M  (~330)    | 29.6%          | 25.1%          | 29.1%          |
| L  (~1000)   | 23.1%          | 27.2%          | 29.6%          |
| XL (~2600)   | 25.5%          | 27.5%          | 25.0% (6 skip) |
| XXL (~6000)  | **42.0%**      | **42.8%**      | **0% (20 skip)** |

Reading the table:

- **Length drives ratio far more than the model.** Short captures (≤90 chars) barely compress — they're already dense — so the default `RTRT_AUTO_COMPRESS_MIN_CHARS=512` correctly skips them. Dense mid-length content sits at ~25-30%; long verbose captures (the bulk of the token weight in real use) reach 40%+.
- **`granite4.1:8b` collapses on very long input** — every 6000-char sample came back no-shorter, so the guard skipped all 20. Fine for mid-length, unfit for the long captures that matter most.
- **Other models disqualified earlier:** `llama3.1:8b` had the highest raw ratio but corrupted facts (changed a 60% to 40%, invented detail); `qwen3.5:9b` is a thinking model and returned every input verbatim (0% across the board); `gemma4:e4b`/`e2b` were weak and injected markdown/LaTeX noise.

**Recommendation: `gemma3:4b` as the local default** — robust across every length (XXL 42%, mid 23-30%), 4.3 GB so it fits 100% on a modest GPU, and it safely skips the short rows. Step up to `gemma3:12b` (10 GB, partial CPU offload) only when you want the marginal quality edge. The code default stays `claude-haiku-4-5` for users with a cloud key; `gemma3:4b` is the recommended local override.

## How to reproduce

```bash
# All criterion suites
cargo bench --workspace

# Just the memory recall bench
cargo bench -p rtrt-memory --bench recall_bench

# Quick run (skip statistical analysis)
cargo bench -p rtrt-memory --bench recall_bench -- --quick

# rtrt CLI shortcut (wraps cargo bench)
rtrt benchmark
rtrt benchmark --bench recall_bench --package rtrt-memory
```

Criterion writes an HTML report to `target/criterion/report/index.html` and a stable text summary to stdout. Both are checked into the PR description for any change in this area.

## Regression policy

- Every PR that touches `crates/rtrt-{compress,memory,proxy}/` re-runs the relevant bench and reports the delta in the PR description.
- A regression beyond **10% on any p50 number** blocks the merge unless an explicit "performance-trade documented" line lands in `CHANGELOG.md`.
- The release workflow runs `cargo bench --workspace` and refuses to publish on regression.
