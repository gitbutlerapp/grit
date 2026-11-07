//! Criterion benchmark: repack on a medium synthetic history fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::maintenance::RepackOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use s7::HistoryBenchFixtures;

fn repack_medium(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("repack_medium");
    group.throughput(Throughput::Elements(fx.commit_count as u64));
    group.bench_function("all_into_one", |b| {
        b.iter(|| {
            black_box(
                fx.repo
                    .maintenance()
                    .repack(
                        &RepackOptions::default(),
                        &mut NullProgress,
                        &CancelToken::never(),
                    )
                    .expect("repack"),
            );
        });
    });
    group.finish();
}

criterion_group!(repack_benches, repack_medium);
criterion_main!(repack_benches);
