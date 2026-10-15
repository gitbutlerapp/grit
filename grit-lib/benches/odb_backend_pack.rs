//! Criterion benchmarks for deep packed-object reads without building 10k/100k backend fixtures.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use fixture::ObjectBenchFixtures;

fn bench_odb_backend_pack_delta(c: &mut Criterion) {
    let pack_fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("odb_backend");
    group.bench_function("read_deep_delta_chain", |b| {
        let odb = &pack_fx.odb;
        let oid = &pack_fx.packed_delta_oid;
        b.iter(|| {
            odb.with_pack_read_context(|| black_box(odb.read(oid).expect("read deep delta chain")))
        });
    });
    group.bench_function("read_info_packed", |b| {
        let odb = &pack_fx.odb;
        let oid = &pack_fx.packed_delta_oid;
        b.iter(|| {
            odb.with_pack_read_context(|| {
                black_box(odb.read_info(oid).expect("read_info packed delta"))
            })
        });
    });
    group.finish();
}

criterion_group!(odb_backend_pack, bench_odb_backend_pack_delta);
criterion_main!(odb_backend_pack);
