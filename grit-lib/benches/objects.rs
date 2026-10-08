//! Criterion micro-benchmarks for object storage, hashing, packs, and deltas.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;
use std::io::Read;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use fixture::ObjectBenchFixtures;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use grit_lib::hash::{hash_object, hash_objects_parallel};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::pack::{clear_pack_cache, read_object_from_pack, PackIndex};
use grit_lib::unpack_objects::apply_delta;
use grit_lib::zlib_inflate::ZlibInflateScratch;

fn bench_sha1_throughput(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("sha1_throughput");
    for (size, buf) in &fx.sha1_buffers {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), buf, |b, input| {
            b.iter(|| {
                let mut hasher = HashAlgo::Sha1.hasher();
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
            b.iter(|| black_box(HashAlgo::Sha1.hash_object(kind, input)));
        });
    }
    group.finish();
}

fn bench_hash_object(c: &mut Criterion) {
    let fx = ObjectBenchFixtures::global();
    let mut group = c.benchmark_group("hash_object");
    for (label, kind, data) in [
        ("blob", ObjectKind::Blob, fx.delta_base.as_slice()),
        ("tree", ObjectKind::Tree, fx.tree_body.as_slice()),
    ] {
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, input| {
            b.iter(|| black_box(hash_object(HashAlgo::Sha1, kind, input)));
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
    for (label, compressed, plain_len) in [
        (
            "tree_200b",
            fx.tree_small_zlib.as_slice(),
            fx.tree_small_store.len(),
        ),
        (
            "blob_4kib",
            fx.blob_4k_zlib.as_slice(),
            fx.blob_4k_store.len(),
        ),
        (
            "blob_1mib",
            fx.blob_1m_zlib.as_slice(),
            fx.blob_1m_store.len(),
        ),
        (
            "delta_payload",
            fx.delta_zlib.as_slice(),
            fx.delta_store.len(),
        ),
        ("blob_typical", fx.blob_zlib.as_slice(), fx.blob_store.len()),
        ("tree_typical", fx.tree_zlib.as_slice(), fx.tree_store.len()),
    ] {
        group.throughput(Throughput::Bytes(compressed.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(compressed, plain_len),
            |b, (input, len)| {
                let mut scratch = ZlibInflateScratch::default();
                b.iter(|| {
                    let mut pos = 0usize;
                    black_box(
                        scratch
                            .decompress_fixed(black_box(input), &mut pos, *len as u64)
                            .expect("inflate"),
                    )
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
            // Criterion warmup fills pack.rs delta-base cache; clear so each sample
            // walks the full verified chain, not a one-hop cached read.
            clear_pack_cache();
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

fn bench_hash_batch_parallel(c: &mut Criterion) {
    const COUNT: usize = 20_000;
    const BLOB_LEN: usize = 16 * 1024;
    let blob = vec![0x5a_u8; BLOB_LEN];
    let items: Vec<(ObjectKind, &[u8])> = (0..COUNT)
        .map(|_| (ObjectKind::Blob, blob.as_slice()))
        .collect();

    let mut group = c.benchmark_group("hash/batch_parallel");
    group.throughput(Throughput::Bytes((COUNT * BLOB_LEN) as u64));
    for threads in [1_usize, 8] {
        let nz = std::num::NonZeroUsize::new(threads).expect("threads");
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("threads={threads}")),
            &nz,
            |b, &threads| {
                b.iter(|| {
                    black_box(hash_objects_parallel(
                        HashAlgo::Sha1,
                        black_box(&items),
                        threads,
                    ))
                });
            },
        );
    }
    group.finish();
}

fn bench_read_info_mib_loose(c: &mut Criterion) {
    static CTX: OnceLock<(tempfile::TempDir, Odb, grit_lib::objects::ObjectId)> = OnceLock::new();
    let (_tmp, odb, oid) = CTX.get_or_init(|| {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(&tmp.path().join("objects"));
        let data = vec![0xABu8; 1024 * 1024];
        let oid = odb
            .write(ObjectKind::Blob, &data)
            .expect("write 1 MiB loose blob");
        (tmp, odb, oid)
    });
    let mut group = c.benchmark_group("read_info_vs_read");
    group.throughput(Throughput::Bytes(1024 * 1024));
    group.bench_function("loose_read", |b| {
        b.iter(|| black_box(odb.read(oid).expect("read")));
    });
    group.bench_function("loose_read_info", |b| {
        b.iter(|| black_box(odb.read_info(oid).expect("read_info")));
    });
    group.finish();
}

fn bench_exists_local_10k_oids_packed(c: &mut Criterion) {
    static FIX: OnceLock<(tempfile::TempDir, Odb, Vec<grit_lib::objects::ObjectId>)> =
        OnceLock::new();
    let (_dir, odb, oids) = FIX.get_or_init(|| fixture::build_packed_exists_local_fixture(10_000));
    c.bench_function("exists_local_10k_oids_packed", |b| {
        b.iter(|| {
            for oid in oids.iter().take(10_000) {
                black_box(odb.exists_local(oid));
            }
        });
    });
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
    bench_hash_batch_parallel,
    bench_object_id_hash,
    bench_hash_object,
    bench_zlib,
    bench_loose_read,
    bench_read_info_mib_loose,
    bench_packed_read,
    bench_idx_lookup,
    bench_exists_local_10k_oids_packed,
    bench_delta_apply
);
criterion_main!(objects);
