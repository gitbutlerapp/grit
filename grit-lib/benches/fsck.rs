//! Repository fsck on a medium synthetic history fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
use grit_lib::progress::{NeverCancel, NullProgress};

fn fsck_medium(c: &mut Criterion) {
    let fx = s7::HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("fsck_medium");
    group.throughput(Throughput::Elements(fx.commit_count as u64));
    group.bench_function("full_repository", |b| {
        b.iter(|| {
            black_box(
                fsck(
                    &fx.repo,
                    &FsckOptions::full_repository(false),
                    &mut NullProgress,
                    &NeverCancel,
                )
                .expect("fsck"),
            )
        });
    });
    group.finish();
}

criterion_group!(fsck_benches, fsck_medium);
criterion_main!(fsck_benches);
