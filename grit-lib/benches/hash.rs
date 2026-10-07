//! Criterion throughput benchmarks for SHA-1 and SHA-256 (`grit_lib::hash`).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::hash::ObjectHasher;
use grit_lib::objects::HashAlgo;

const SIZES: &[(usize, &str)] = &[
    (64, "64B"),
    (4 * 1024, "4KiB"),
    (1024 * 1024, "1MiB"),
    (64 * 1024 * 1024, "64MiB"),
];

fn bench_digest(c: &mut Criterion, algo: HashAlgo, group_name: &str) {
    let mut group = c.benchmark_group(group_name);
    for &(size, label) in SIZES {
        let data = vec![0x5a_u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, input| {
            b.iter(|| {
                let mut hasher = ObjectHasher::new(algo);
                hasher.update(black_box(input.as_slice()));
                black_box(hasher.finalize())
            });
        });
    }
    group.finish();
}

fn bench_sha1(c: &mut Criterion) {
    bench_digest(c, HashAlgo::Sha1, "hash_sha1");
}

fn bench_sha256(c: &mut Criterion) {
    bench_digest(c, HashAlgo::Sha256, "hash_sha256");
}

criterion_group!(benches, bench_sha1, bench_sha256);
criterion_main!(benches);
