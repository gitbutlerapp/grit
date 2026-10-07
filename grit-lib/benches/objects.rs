//! Criterion micro-benchmarks for object storage, hashing, packs, and deltas.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;
use std::io::Read;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;
use grit_lib::pack::{read_object_from_pack, PackIndex};
use grit_lib::unpack_objects::apply_delta;
use sha1::{Digest, Sha1};

use fixture::ObjectBenchFixtures;

fn bench_sha1_throughput(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("sha1_throughput");
    for (size, buf) in &fx.sha1_buffers {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), buf, |b, input| {
            b.iter(|| {
                let mut hasher = Sha1::new();
                hasher.update(black_box(input.as_slice()));
                hasher.finalize()
            });
        });
    }
    group.finish();
}

fn bench_object_id_hash(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("object_id_hash");
    for (label, kind, data) in [
        ("blob", ObjectKind::Blob, fx.delta_base.as_slice()),
        ("tree", ObjectKind::Tree, fx.tree_body.as_slice()),
    ] {
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, input| {
            b.iter(|| black_box(Odb::hash_object_data(kind, input)));
        });
    }
    group.finish();
}

fn bench_zlib(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("zlib_deflate");
    for (label, store) in [
        ("blob", fx.blob_store.as_slice()),
        ("tree", fx.tree_store.as_slice()),
    ] {
        group.throughput(Throughput::Bytes(store.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), store, |b, input| {
            b.iter(|| {
                let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
                std::io::Write::write_all(&mut enc, black_box(input)).expect("deflate");
                enc.finish().expect("finish")
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("zlib_inflate");
    for (label, compressed) in [
        ("blob", fx.blob_zlib.as_slice()),
        ("tree", fx.tree_zlib.as_slice()),
    ] {
        group.throughput(Throughput::Bytes(compressed.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            compressed,
            |b, input| {
                b.iter(|| {
                    let mut out = Vec::new();
                    ZlibDecoder::new(black_box(input))
                        .read_to_end(&mut out)
                        .expect("inflate");
                    out
                });
            },
        );
    }
    group.finish();
}

fn bench_loose_read(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    c.bench_function("loose_object_read", |b| {
        b.iter(|| black_box(fx.odb.read(&fx.loose_blob_oid).expect("read loose")));
    });
}

fn bench_packed_read(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("packed_object_read");
    group.bench_function("whole", |b| {
        b.iter(|| {
            black_box(
                read_object_from_pack(&fx.packed_whole_idx, &fx.packed_whole_oid)
                    .expect("read whole"),
            )
        });
    });
    group.bench_function("deep_delta_chain", |b| {
        assert!(
            fx.packed_delta_chain_depth >= 50,
            "fixture delta depth {}",
            fx.packed_delta_chain_depth
        );
        b.iter(|| {
            black_box(
                read_object_from_pack(&fx.packed_delta_idx, &fx.packed_delta_oid)
                    .expect("read delta chain"),
            )
        });
    });
    group.finish();
}

fn bench_idx_lookup(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("pack_idx_lookup");

    fn lookup_hit(idx: &PackIndex, oid: &grit_lib::objects::ObjectId) {
        black_box(idx.find_offset(oid));
    }
    fn lookup_miss(idx: &PackIndex, oid: &grit_lib::objects::ObjectId) {
        black_box(idx.find_offset(oid));
    }

    for (name, idx, hit, miss) in [
        ("small_hit", &fx.small_idx, &fx.small_hit, &fx.small_miss),
        ("small_miss", &fx.small_idx, &fx.small_miss, &fx.small_miss),
        ("large_hit", &fx.large_idx, &fx.large_hit, &fx.large_miss),
        ("large_miss", &fx.large_idx, &fx.large_miss, &fx.large_miss),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), idx, |b, index| {
            b.iter(|| {
                if name.ends_with("_hit") {
                    lookup_hit(index, hit);
                } else {
                    lookup_miss(index, miss);
                }
            });
        });
    }
    group.finish();
}

fn bench_delta_apply(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("delta_apply");
    group.throughput(Throughput::Bytes(fx.delta_base.len() as u64));
    group.bench_function("apply", |b| {
        b.iter(|| {
            black_box(
                apply_delta(black_box(&fx.delta_base), black_box(&fx.delta_bytes)).expect("apply"),
            )
        });
    });
    group.finish();
}

criterion_group!(
    objects,
    bench_sha1_throughput,
    bench_object_id_hash,
    bench_zlib,
    bench_loose_read,
    bench_packed_read,
    bench_idx_lookup,
    bench_delta_apply
);
criterion_main!(objects);
