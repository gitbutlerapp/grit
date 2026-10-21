//! Pack index mmap open and OID lookup on the repacked 100k fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use grit_lib::objects::pack::{clear_pack_cache, read_pack_index_no_verify};
use grit_lib::objects::ObjectId;

struct IdxFixture {
    idx_path: std::path::PathBuf,
    sample_oids: Vec<ObjectId>,
}

fn idx_fixture() -> &'static IdxFixture {
    static FIX: OnceLock<IdxFixture> = OnceLock::new();
    FIX.get_or_init(|| {
        let fx = fixture::OdbBackendFixtures::global();
        let pack_dir = fx.pack_100k.odb.objects_dir().join("pack");
        let idx_path = std::fs::read_dir(&pack_dir)
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "idx"))
            .expect("idx file");
        let idx = read_pack_index_no_verify(&idx_path).expect("open idx");
        let step = (idx.len() / 1000).max(1);
        let sample_oids: Vec<ObjectId> = (0..idx.len())
            .step_by(step)
            .take(1000)
            .map(|i| ObjectId::from_bytes(idx.oid_at(i)).expect("oid"))
            .collect();
        IdxFixture {
            idx_path,
            sample_oids,
        }
    })
}

fn bench_pack_index_open_and_lookups(c: &mut Criterion) {
    let fx = idx_fixture();
    let mut group = c.benchmark_group("pack_index/open_100k");
    group.throughput(Throughput::Elements(1000));
    group.bench_function("open_no_verify", |b| {
        b.iter(|| {
            clear_pack_cache();
            black_box(read_pack_index_no_verify(&fx.idx_path).expect("open"))
        });
    });
    group.bench_function("open_cached_1000_find_offset", |b| {
        b.iter_batched(
            || read_pack_index_no_verify(&fx.idx_path).expect("open"),
            |idx| {
                for oid in &fx.sample_oids {
                    black_box(idx.find_offset(oid));
                }
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(pack_index_open_benches, bench_pack_index_open_and_lookups);
criterion_main!(pack_index_open_benches);
