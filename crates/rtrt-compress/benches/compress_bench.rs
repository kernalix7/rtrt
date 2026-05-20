use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;
use std::hint::black_box;

const FIXTURES: &[(&str, &str)] = &[
    ("short", include_str!("fixtures/short.md")),
    ("code", include_str!("fixtures/code.md")),
    ("mixed", include_str!("fixtures/mixed.md")),
    ("long", include_str!("fixtures/long.md")),
];

const LEVELS: &[(&str, CompressionLevel)] = &[
    ("lite", CompressionLevel::Lite),
    ("full", CompressionLevel::Full),
    ("ultra", CompressionLevel::Ultra),
];

fn compress_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("compress");
    for (fixture_name, body) in FIXTURES {
        for (level_name, level) in LEVELS {
            let compressor = Compressor::new(*level);
            group.throughput(criterion::Throughput::Bytes(body.len() as u64));
            group.bench_with_input(
                BenchmarkId::new(*level_name, *fixture_name),
                body,
                |b, body| b.iter(|| black_box(compressor.compress(black_box(body)))),
            );
        }
    }
    group.finish();
}

criterion_group!(benches, compress_throughput);
criterion_main!(benches);
