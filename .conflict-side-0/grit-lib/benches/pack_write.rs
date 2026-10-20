//! Criterion benchmarks for pack-objects on the ~100k-object repacked fixture:
//! on-disk delta/object reuse vs fresh windowed delta search.

#![allow(clippy::expect_used, clippy::unwrap_used, dead_code)]

mod fixture;

use std::hint::black_box;
use std::sync::OnceLock;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use fixture::OdbBackendFixtures;
use grit_lib::objects::pack_objects::{build_pack_with_parallel_threads, PackBuildOptions};
use grit_lib::objects::ObjectId;

struct PackWriteBench {
    root: ObjectId,
    /// Object count in the emitted pack when rebuilding the full closure from [`Self::root`].
    emitted_object_count: u64,
    reuse_opts: PackBuildOptions,
    fresh_opts: PackBuildOptions,
}

fn pack_header_object_count(pack: &[u8]) -> u32 {
    assert!(pack.len() >= 12 && &pack[0..4] == b"PACK");
    u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]])
}

fn pack_write_bench() -> &'static PackWriteBench {
    static BENCH: OnceLock<PackWriteBench> = OnceLock::new();
    BENCH.get_or_init(|| {
        let fx = OdbBackendFixtures::global();
        let case = &fx.pack_100k;
        let root = case.history_tip;
        let reuse_opts = PackBuildOptions {
            use_bitmaps: false,
            ..PackBuildOptions::for_local_copy(None)
        };
        let fresh_opts = PackBuildOptions {
            delta: true,
            reuse_objects: false,
            reuse_deltas: false,
            use_bitmaps: false,
            ..PackBuildOptions::default()
        };

        let reuse_pack = build_pack_with_parallel_threads(
            &case.odb,
            &[root],
            &[],
            &reuse_opts,
            None,
            None,
        )
        .expect("probe reuse pack from history tip");
        let fresh_pack = build_pack_with_parallel_threads(
            &case.odb,
            &[root],
            &[],
            &fresh_opts,
            None,
            None,
        )
        .expect("probe fresh pack from history tip");

        let reuse_count = pack_header_object_count(&reuse_pack);
        let fresh_count = pack_header_object_count(&fresh_pack);
        assert_eq!(
            reuse_count, fresh_count,
            "reuse and fresh paths must emit the same object set size (reuse={reuse_count}, fresh={fresh_count})"
        );
        assert!(
            reuse_count >= 100_000,
            "expected a large closure from history tip {root}, pack header count {reuse_count}"
        );

        PackWriteBench {
            root,
            emitted_object_count: u64::from(reuse_count),
            reuse_opts,
            fresh_opts,
        }
    })
}

fn bench_pack_write_100k(c: &mut Criterion) {
    let fx = OdbBackendFixtures::global();
    let case = &fx.pack_100k;
    let meta = pack_write_bench();
    assert_eq!(
        meta.root, case.history_tip,
        "benchmark root must stay the fixture history tip"
    );

    let mut group = c.benchmark_group("pack");
    group.throughput(Throughput::Elements(meta.emitted_object_count));

    group.bench_function("write_100k_reuse", |b| {
        b.iter(|| {
            let pack = build_pack_with_parallel_threads(
                &case.odb,
                &[meta.root],
                &[],
                &meta.reuse_opts,
                None,
                None,
            )
            .expect("reuse pack");
            assert_eq!(
                pack_header_object_count(&pack) as u64,
                meta.emitted_object_count,
                "reuse pack object count must match throughput baseline"
            );
            black_box(pack)
        });
    });

    group.bench_function("write_100k_fresh", |b| {
        b.iter(|| {
            let pack = build_pack_with_parallel_threads(
                &case.odb,
                &[meta.root],
                &[],
                &meta.fresh_opts,
                None,
                None,
            )
            .expect("fresh pack");
            assert_eq!(
                pack_header_object_count(&pack) as u64,
                meta.emitted_object_count,
                "fresh pack object count must match throughput baseline"
            );
            black_box(pack)
        });
    });

    group.finish();
}

criterion_group!(pack_write, bench_pack_write_100k);
criterion_main!(pack_write);
