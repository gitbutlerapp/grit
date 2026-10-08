//! Criterion benchmarks focused on pack index lookup and packed object reads.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;
use std::io::Read;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use fixture::{build_deep_repack_read_sample, ObjectBenchFixtures};
use flate2::read::ZlibDecoder;
use grit_lib::pack::{clear_pack_cache, read_object_from_pack, read_pack_index};

fn bench_idx_lookup(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("odb_read/idx_lookup");
    for (name, idx, hit, miss) in [
        ("large_hit", &fx.large_idx, &fx.large_hit, &fx.large_miss),
        ("large_miss", &fx.large_idx, &fx.large_miss, &fx.large_miss),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), idx, |b, index| {
            b.iter(|| {
                if name.ends_with("_hit") {
                    black_box(index.find_offset(hit));
                } else {
                    black_box(index.find_offset(miss));
                }
            });
        });
    }
    group.finish();
}

fn bench_packed_read(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("odb_read/packed_read");
    group.bench_function("whole_object", |b| {
        b.iter(|| {
            black_box(
                read_object_from_pack(&fx.packed_whole_idx, &fx.packed_whole_oid)
                    .expect("read whole"),
            )
        });
    });
    group.bench_function("deep_delta_chain", |b| {
        clear_pack_cache();
        b.iter(|| {
            clear_pack_cache();
            black_box(
                read_object_from_pack(&fx.packed_delta_idx, &fx.packed_delta_oid)
                    .expect("read delta chain"),
            )
        });
    });
    group.finish();
}

fn bench_delta_apply(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    c.bench_function("odb_read/delta_apply", |b| {
        b.iter(|| {
            black_box(
                grit_lib::unpack_objects::apply_delta(
                    black_box(&fx.delta_base),
                    black_box(&fx.delta_bytes),
                )
                .expect("apply"),
            )
        });
    });
}

fn bench_inflate(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    c.bench_function("odb_read/inflate_blob", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            ZlibDecoder::new(black_box(fx.blob_zlib.as_slice()))
                .read_to_end(&mut out)
                .expect("inflate");
            black_box(out)
        });
    });
}

fn bench_open_pack_read_one(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let idx_path = fx.packed_whole_idx.idx_path.clone();
    let oid = fx.packed_whole_oid;
    c.bench_function("odb_read/open_pack_read_one", |b| {
        b.iter(|| {
            clear_pack_cache();
            let idx = read_pack_index(&idx_path).expect("open idx");
            black_box(read_object_from_pack(&idx, &oid).expect("read one"));
        });
    });
}

fn bench_repack_deep_chain(c: &mut Criterion) {
    static DEEP: OnceLock<(tempfile::TempDir, grit_lib::pack::PackIndex, grit_lib::objects::ObjectId)> =
        OnceLock::new();
    let (_keep, idx, sample_oid) = DEEP.get_or_init(build_deep_repack_read_sample);
    c.bench_function("odb_read/deep_repack_read_one", |b| {
        b.iter(|| {
            clear_pack_cache();
            black_box(read_object_from_pack(idx, sample_oid).expect("read"));
        });
    });
}

criterion_group!(
    odb_read,
    bench_idx_lookup,
    bench_packed_read,
    bench_delta_apply,
    bench_inflate,
    bench_open_pack_read_one,
    bench_repack_deep_chain
);
criterion_main!(odb_read);
