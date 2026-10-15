//! Criterion benchmarks for deep packed-object reads without building 10k/100k backend fixtures.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "fixture_pack_delta.rs"]
mod pack_delta_fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};

fn bench_odb_backend_pack_delta(c: &mut Criterion) {
    let pack_fx = pack_delta_fixture::global();
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
