//! Criterion benchmarks for [`Odb`] read, metadata, existence, and write paths
//! across loose, pack, MIDX, and alternates layouts (pre-ObjectStore refactor baseline).

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use fixture::{OdbBackendCase, OdbBackendFixtures};
use grit_lib::objects::{ObjectId, ObjectKind};

fn fixtures() -> &'static OdbBackendFixtures {
    OdbBackendFixtures::global()
}

fn bench_case(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    case: &OdbBackendCase,
) {
    let odb = &case.odb;
    let hit = case.hit;
    let miss = case.miss;
    let write_seq = AtomicU64::new(0);

    group.bench_with_input(BenchmarkId::new("read", name), &hit, |b, oid| {
        b.iter(|| black_box(odb.read(oid).expect("read hit")));
    });
    group.bench_with_input(BenchmarkId::new("read_info", name), &hit, |b, oid| {
        b.iter(|| black_box(odb.read_info(oid).expect("read_info hit")));
    });
    group.bench_with_input(BenchmarkId::new("exists_hit", name), &hit, |b, oid| {
        b.iter(|| black_box(odb.exists(oid)));
    });
    group.bench_with_input(BenchmarkId::new("exists_miss", name), &miss, |b, oid| {
        b.iter(|| black_box(odb.exists(oid)));
    });
    group.bench_with_input(BenchmarkId::new("write", name), odb, |b, odb| {
        b.iter(|| {
            let n = write_seq.fetch_add(1, Ordering::Relaxed);
            let body = format!("odb-backend-write-{name}-{n}\n");
            black_box(
                odb.write(ObjectKind::Blob, body.as_bytes())
                    .expect("write loose"),
            )
        });
    });
}

fn bench_odb_backend(c: &mut Criterion) {
    let fx = fixtures();
    let mut group = c.benchmark_group("odb_backend");
    bench_case(&mut group, "loose_10k", &fx.loose_10k);
    bench_case(&mut group, "pack_100k", &fx.pack_100k);
    bench_case(&mut group, "midx_8", &fx.midx_8);
    bench_case(&mut group, "alternate_only", &fx.alternate_only);
    group.finish();
}

fn miss_oid_batch(base: &ObjectId, count: usize) -> Vec<ObjectId> {
    let bytes = base.as_bytes();
    (0..count)
        .map(|i| {
            let mut b = [0u8; 20];
            b.copy_from_slice(bytes);
            b[16] = (i >> 8) as u8;
            b[17] = (i & 0xff) as u8;
            b[18] = ((i >> 16) & 0xff) as u8;
            b[19] = ((i >> 24) & 0xff) as u8;
            ObjectId::from_bytes(&b).expect("synthetic miss oid")
        })
        .collect()
}

fn bench_exists_miss_loop(c: &mut Criterion) {
    static MISSES: OnceLock<Vec<ObjectId>> = OnceLock::new();
    let fx = fixtures();
    let odb = &fx.pack_100k.odb;
    let misses = MISSES.get_or_init(|| miss_oid_batch(&fx.pack_100k.miss, 10_000));
    c.bench_function("odb_backend/exists_miss_loop_10k", |b| {
        b.iter(|| {
            for oid in misses {
                black_box(odb.exists(oid));
            }
        });
    });
}

criterion_group!(odb_backend, bench_odb_backend, bench_exists_miss_loop);
criterion_main!(odb_backend);
