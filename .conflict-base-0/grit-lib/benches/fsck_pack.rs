//! Pack-heavy fsck on the 100k-object ODB backend fixture.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::maintenance::repository_fsck::{fsck, FsckOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;

fn fsck_pack_100k(c: &mut Criterion) {
    let fx = fixture::OdbBackendFixtures::global();
    let git_dir = fx.pack_100k.odb.objects_dir().parent().expect("git dir");
    let repo = Repository::open(git_dir, &RepositoryOptions::empty()).expect("open pack repo");
    let mut group = c.benchmark_group("fsck/pack_100k");
    group.throughput(Throughput::Elements(100_000));
    group.bench_function("full_repository", |b| {
        b.iter_batched(
            || (),
            |_| {
                black_box(
                    fsck(
                        &repo,
                        &FsckOptions::full_repository(false),
                        &mut NullProgress,
                        &NeverCancel,
                    )
                    .expect("fsck"),
                )
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(fsck_pack_benches, fsck_pack_100k);
criterion_main!(fsck_pack_benches);
