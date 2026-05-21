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

Quality metrics require labelled datasets. These are aspirational and tracked under the `rtrt-eval` opt-in crate once it ships.

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
| `recent_paged` (limit=50) | 1 K rows | **815 µs** | ✅ |
| `recent_paged` (limit=50) | 10 K rows | **8.1 ms** | ✅ |
| `recent_paged` (limit=50) | 100 K rows | **71 ms** | ⚠ above 15 ms timeline target |
| `save_one` | 1 K rows | **25 µs** | ✅ (target 2 ms) |
| `save_one` | 10 K rows | **26 µs** | ✅ |
| `projects_listing` | 8 projects × 1 K | **629 µs** | ✅ |

**Notes**

- `recall_bm25` stays under the SLO at every size — FTS5 is doing its job.
- `recent_paged` at 100 K is the obvious next target. The query is `ORDER BY created_at DESC, id DESC LIMIT N OFFSET M`. The `created_at` index helps the head, but deep `OFFSET` pages still scan. Plan: add a covering index on `(project, created_at DESC, id DESC)` and revisit.
- `save_one` is constant — the WAL journal absorbs writes.

### `rtrt-compress` benchmark — last published

See `crates/rtrt-compress/benches/compress_bench.rs`. The README's "60%+ savings" claim is measured here per fixture × level. Refresh with `rtrt benchmark`.

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
