//! Index-pack delta resolution on a 100k-object repacked fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::objects::hash::Parallelism;
use grit_lib::objects::index_pack::build_pack_index_records;
use grit_lib::objects::odb::Odb;

fn index_pack_resolve_100k(c: &mut Criterion) {
    let fx = fixture::OdbBackendFixtures::global();
    let odb = &fx.pack_100k.odb;
    let pack_path = odb
        .objects_dir()
        .join("pack")
        .read_dir()
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .expect("pack file")
        .path();
    let pack_bytes = std::fs::read(&pack_path).expect("read pack");
    let mut group = c.benchmark_group("index_pack/resolve_100k");
    group.throughput(Throughput::Elements(100_000));
    group.bench_function("build_records_tauto", |b| {
        b.iter(|| {
            black_box(
                build_pack_index_records(&pack_bytes, odb, Parallelism::resolve(None), false)
                    .expect("index records"),
            )
        });
    });
    group.finish();
}

criterion_group!(index_pack_resolve_benches, index_pack_resolve_100k);
criterion_main!(index_pack_resolve_benches);
