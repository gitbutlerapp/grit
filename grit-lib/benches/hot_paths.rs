//! Criterion hot-path benchmarks (index, checkout, staging scan) at L/H scale.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod hot_paths_fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use hot_paths_fixture::{
    bench_apply_stash, bench_checkout_between_trees, bench_pick_path, bench_push_stash,
    bench_stage_scan, bench_switch_wide, HotPathsFixture,
};

fn bench_index_mutate(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_mutate_batch_10pct");
    for (label, fx) in [
        ("L", HotPathsFixture::large()),
        ("H", HotPathsFixture::heavy()),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(label), fx, |b, fixture| {
            b.iter(|| black_box(fixture.apply_index_mutate_batch().entries().len()));
        });
    }
    group.finish();

    let mut fsmn_group = c.benchmark_group("index_mutate_batch_10pct_fsmn");
    for (label, fx) in [
        ("L", HotPathsFixture::large()),
        ("H", HotPathsFixture::heavy()),
    ] {
        fsmn_group.bench_with_input(BenchmarkId::from_parameter(label), fx, |b, fixture| {
            b.iter(|| black_box(fixture.apply_index_mutate_batch_fsmn().entries().len()));
        });
    }
    fsmn_group.finish();
}

fn bench_switch_wide_100k(c: &mut Criterion) {
    let fx = HotPathsFixture::heavy();
    let mut group = c.benchmark_group("checkout/switch_wide_100k");
    group.bench_function("H_10pct_switch", |b| {
        b.iter_batched(
            || fx.reset_for_switch_benchmark(),
            |_| bench_switch_wide(fx),
            BatchSize::SmallInput,
        );
    });
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

fn bench_push_stash_group(c: &mut Criterion) {
    let fx = HotPathsFixture::large();
    let mut group = c.benchmark_group("push_stash");
    group.bench_function("stash_push_L", |b| {
        b.iter_batched(
            || fx.reset_worktree_to_head(),
            |_| bench_push_stash(fx),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_apply_stash_group(c: &mut Criterion) {
    let fx = HotPathsFixture::large();
    let stash_oid = fx.build_stash_touching_fraction(0.01);
    let mut group = c.benchmark_group("apply_stash");
    group.bench_function("apply_stash_L", |b| {
        b.iter_batched(
            || fx.reset_worktree_to_head(),
            |_| bench_apply_stash(fx, &stash_oid),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_pick_paths(c: &mut Criterion) {
    let fx = HotPathsFixture::large();
    let mut group = c.benchmark_group("pick_paths");
    group.bench_function("pick_2000_paths_L", |b| {
        b.iter_batched(
            || fx.reset_worktree_to_head(),
            |_| bench_pick_path(fx, 2000),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_status_clean_100k(c: &mut Criterion) {
    let fx = HotPathsFixture::heavy();
    fx.ensure_clean_stat_cache();
    let mut group = c.benchmark_group("status/clean_100k");
    group.bench_function("grit_lib", |b| {
        b.iter(|| black_box(fx.bench_status_clean()));
    });
    group.finish();
}

fn bench_diff_worktree_clean_10k(c: &mut Criterion) {
    let fx = HotPathsFixture::large();
    fx.ensure_clean_stat_cache();
    let mut group = c.benchmark_group("diff/worktree_clean_10k");
    group.bench_function("grit_lib", |b| {
        b.iter(|| black_box(fx.bench_diff_worktree_clean()));
    });
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
    targets = bench_index_mutate,
    bench_checkout,
    bench_status_clean_100k,
    bench_diff_worktree_clean_10k,
    bench_switch_wide_100k,
    bench_staging_scan,
    bench_push_stash_group,
    bench_apply_stash_group,
    bench_pick_paths
);
criterion_main!(hot_paths);

#[cfg(test)]
mod smoke {
    use super::hot_paths_fixture::{bench_switch_wide, HotPathsFixture};

    #[test]
    fn switch_wide_roundtrip_on_large_fixture() {
        let fx = HotPathsFixture::large();
        fx.reset_for_switch_benchmark();
        bench_switch_wide(fx);
        fx.reset_for_switch_benchmark();
        bench_switch_wide(fx);
    }

    #[test]
    #[ignore = "slow: builds 100k-file fixture"]
    fn switch_wide_roundtrip_on_heavy_fixture() {
        let fx = HotPathsFixture::heavy();
        fx.reset_for_switch_benchmark();
        bench_switch_wide(fx);
        fx.reset_for_switch_benchmark();
        bench_switch_wide(fx);
    }
}
