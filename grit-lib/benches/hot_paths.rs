//! Criterion hot-path benchmarks (index, checkout, staging scan) at L/H scale.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod hot_paths_fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use hot_paths_fixture::{bench_checkout_between_trees, bench_stage_scan, HotPathsFixture};

fn bench_index_mutate(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_mutate_batch_10pct");
    for (label, fx) in [
        ("L", HotPathsFixture::large()),
        ("H", HotPathsFixture::heavy()),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(label), fx, |b, fixture| {
            b.iter(|| black_box(fixture.apply_index_mutate_batch().entries.len()));
        });
    }
    group.finish();
}

fn bench_checkout(c: &mut Criterion) {
    let mut group = c.benchmark_group("checkout_between_trees");
    for (label, fx) in [
        ("L", HotPathsFixture::large()),
        ("H", HotPathsFixture::heavy()),
    ] {
        for pct in [0.01_f64, 0.10] {
            let to_tree = fx.tree_with_fraction_changed(pct);
            let id = format!("{label}_{pct}");
            group.bench_with_input(BenchmarkId::from_parameter(id), fx, |b, fixture| {
                fixture.reset_worktree_to_head();
                b.iter(|| {
                    bench_checkout_between_trees(&fixture.repo, &fixture.head_tree, &to_tree);
                });
            });
        }
    }
    group.finish();
}

fn bench_staging_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("staging_scan_5_modified");
    for (label, fx) in [
        ("L", HotPathsFixture::large()),
        ("H", HotPathsFixture::heavy()),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(label), fx, |b, fixture| {
            b.iter_batched(
                || {
                    fixture.reset_worktree_to_head();
                    fixture.modify_worktree_files(5);
                },
                |_| bench_stage_scan(&fixture.repo),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(
    name = hot_paths;
    config = Criterion::default().sample_size(10);
    targets = bench_index_mutate, bench_checkout, bench_staging_scan
);
criterion_main!(hot_paths);
