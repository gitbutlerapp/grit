//! Criterion throughput benchmarks for SHA-1 and SHA-256 (`grit_lib::objects::hash`).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use std::io::Write as _;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::objects::hash::{HashingWriter, ObjectHasher};
use grit_lib::objects::{HashAlgo, ObjectKind};

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

const OBJECT_ID_SIZES: &[(usize, &str)] = &[
    (4 * 1024, "4KiB"),
    (1024 * 1024, "1MiB"),
    (64 * 1024 * 1024, "64MiB"),
];

const CHECKSUM_WRITER_SIZES: &[(usize, &str)] =
    &[(1024 * 1024, "1MiB"), (64 * 1024 * 1024, "64MiB")];

fn bench_hash_object_sha1(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_object_sha1");
    for &(size, label) in OBJECT_ID_SIZES {
        let data = vec![0x5a_u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, input| {
            b.iter(|| {
                let mut hasher =
                    ObjectHasher::for_object(HashAlgo::Sha1, ObjectKind::Blob, input.len() as u64);
                hasher.update(black_box(input.as_slice()));
                black_box(hasher.finalize())
            });
        });
    }
    group.finish();
}

fn bench_hashing_writer_sha1(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashing_writer_sha1");
    for &(size, label) in CHECKSUM_WRITER_SIZES {
        let data = vec![0x5a_u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, input| {
            b.iter(|| {
                let mut writer = HashingWriter::new(HashAlgo::Sha1, std::io::sink());
                writer
                    .write_all(black_box(input.as_slice()))
                    .expect("write to sink");
                black_box(writer.finish().expect("finish hashing writer"))
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_sha1,
    bench_sha256,
    bench_hash_object_sha1,
    bench_hashing_writer_sha1
);
criterion_main!(benches);
