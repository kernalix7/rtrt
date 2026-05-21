//! Recall latency benchmarks.
//!
//! Builds a synthetic in-memory store at three sizes (1k / 10k / 100k rows)
//! and measures BM25 / paginated timeline / project listing / graph walk.
//! Run with `cargo bench -p rtrt-memory --bench recall_bench`.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rtrt_memory::MemoryStore;

const PROJECT: &str = "bench";

/// Deterministic body generator so runs are comparable across commits.
fn synth_body(i: usize) -> String {
    // Mix common BM25 tokens with row-unique tokens so recall actually
    // discriminates instead of returning every row for any query.
    let keywords = [
        "auth", "parser", "retry", "cache", "token", "stream", "memory", "graph", "vector",
        "filter", "compress", "schema", "session", "block", "embed",
    ];
    let kw = keywords[i % keywords.len()];
    let neighbour = keywords[(i + 3) % keywords.len()];
    format!(
        "row {i} discusses {kw} and {neighbour}. lorem ipsum dolor sit amet \
         consectetur adipiscing elit sed do eiusmod tempor incididunt."
    )
}

fn seed(rows: usize) -> MemoryStore {
    let store = MemoryStore::open_in_memory().expect("open in-memory store");
    for i in 0..rows {
        let body = synth_body(i);
        let kind = if i % 7 == 0 { "decision" } else { "note" };
        let _ = store.save(PROJECT, kind, &body);
    }
    store
}

fn bm25(c: &mut Criterion) {
    let mut group = c.benchmark_group("recall_bm25");
    for size in [1_000usize, 10_000, 100_000] {
        let store = seed(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let _ = store.recall_bm25(PROJECT, "auth retry", 5);
            });
        });
    }
    group.finish();
}

fn timeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("recent_paged");
    for size in [1_000usize, 10_000, 100_000] {
        let store = seed(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let _ = store.recent_paged(PROJECT, 50, 0);
            });
        });
    }
    group.finish();
}

fn save_one(c: &mut Criterion) {
    let mut group = c.benchmark_group("save_one");
    for size in [1_000usize, 10_000] {
        let store = seed(size);
        let mut i = size;
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                let _ = store.save(PROJECT, "note", &synth_body(i));
                i += 1;
            });
        });
    }
    group.finish();
}

fn projects_listing(c: &mut Criterion) {
    // 8 projects × 1k rows each.
    let store = MemoryStore::open_in_memory().expect("open");
    for p in 0..8 {
        for i in 0..1_000 {
            let _ = store.save(&format!("proj_{p}"), "note", &synth_body(i));
        }
    }
    c.bench_function("projects_listing", |b| {
        b.iter(|| {
            let _ = store.projects();
        });
    });
}

criterion_group!(benches, bm25, timeline, save_one, projects_listing);
criterion_main!(benches);
