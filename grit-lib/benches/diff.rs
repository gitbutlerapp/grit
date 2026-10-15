//! Criterion micro-benchmarks for tree and blob diff hot paths.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::{count_changes_git_histogram, count_changes_with_algorithm, diff_trees};
use similar::Algorithm;

use s7::HistoryBenchFixtures;

fn bench_tree_diff_wide(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("diff/tree_diff_wide");
    for (name, old, new) in [
        ("few", fx.wide_tree, fx.wide_tree_few_changes),
        ("many", fx.wide_tree, fx.wide_tree_many_changes),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), &(old, new), |b, pair| {
            b.iter(|| {
                black_box(
                    diff_trees(&fx.repo.odb(), Some(&pair.0), Some(&pair.1), "")
                        .expect("tree diff"),
                )
            });
        });
    }
    group.finish();
}

fn bench_blob_myers_10k_lines(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let old = fx.blob_large_old.as_str();
    let new = fx.blob_large_new.as_str();
    let mut group = c.benchmark_group("diff/blob_myers_10k_lines");
    group.throughput(Throughput::Bytes(old.len() as u64 + new.len() as u64));
    group.bench_function("myers", |b| {
        b.iter(|| {
            black_box(count_changes_with_algorithm(
                old,
                new,
                Algorithm::Myers,
                false,
            ))
        });
    });
    group.bench_function("histogram", |b| {
        b.iter(|| black_box(count_changes_git_histogram(old, new)));
    });
    group.finish();
}

criterion_group!(
    diff_benches,
    bench_tree_diff_wide,
    bench_blob_myers_10k_lines
);
criterion_main!(diff_benches);
