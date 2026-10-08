//! Criterion benchmarks for revwalk, rev-parse, tree diff, and blob diff.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s1;
mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::{count_changes_with_algorithm, diff_trees};
use grit_lib::rev_list::{rev_list, OrderingMode, RevListOptions};
use grit_lib::rev_parse::resolve_revision;
use similar::Algorithm;

use s7::HistoryBenchFixtures;

fn revwalk_group(c: &mut Criterion, ordering: OrderingMode, label: &str) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group(format!("revwalk_{label}"));
    group.throughput(Throughput::Elements(fx.commit_count as u64));

    for (graph_label, use_graph) in [("no_commit_graph", false), ("commit_graph", true)] {
        group.bench_with_input(
            BenchmarkId::new("walk", graph_label),
            &use_graph,
            |b, with_graph| {
                if *with_graph {
                    fx.ensure_commit_graph();
                } else {
                    fx.remove_commit_graph();
                }
                b.iter(|| {
                    let opts = RevListOptions {
                        ordering,
                        max_count: None,
                        use_commit_graph: *with_graph,
                        ..Default::default()
                    };
                    let result =
                        rev_list(&fx.repo, &[String::from("HEAD")], &[], &opts).expect("rev-list");
                    black_box(result.commits.len())
                });
            },
        );
    }
    group.finish();
}

fn bench_s1_rev_list_objects_v1(c: &mut Criterion) {
    if !s1::fixture_available() {
        return;
    }
    let fx = s1::S1Fixture::global();
    let mut group = c.benchmark_group("s1");
    group.sample_size(7);
    group.bench_function("rev_list_objects_v1.0.0", |b| {
        b.iter(|| black_box(s1::run_rev_list_objects_v1(&fx.repo)));
    });
    group.finish();
}

fn bench_revwalk_topo(c: &mut Criterion) {
    revwalk_group(c, OrderingMode::Topo, "topo");
}

fn bench_revwalk_date(c: &mut Criterion) {
    revwalk_group(c, OrderingMode::DateOrderWalk, "date_order");
}

fn bench_rev_parse(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let head_hex = fx.head.to_string();
    let short = head_hex.chars().take(7).collect::<String>();
    let specs = [
        ("full_oid", head_hex.clone()),
        ("short_oid", short),
        ("branch", "main".to_owned()),
        ("head_tilde", format!("HEAD~{}", s7::head_tilde_distance())),
        ("tag_tree", "bench-tag^{tree}".to_owned()),
        ("head_caret0", "HEAD^0".to_owned()),
    ];
    let mut group = c.benchmark_group("rev_parse");
    for (name, spec) in specs {
        group.bench_with_input(BenchmarkId::from_parameter(name), &spec, |b, spec| {
            b.iter(|| black_box(resolve_revision(&fx.repo, spec).expect("rev-parse")));
        });
    }
    group.finish();
}

fn bench_tree_diff(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("tree_diff");
    let cases = [
        ("wide_few", fx.wide_tree, fx.wide_tree_few_changes),
        ("wide_many", fx.wide_tree, fx.wide_tree_many_changes),
        ("deep_few", fx.deep_tree, fx.deep_tree_few_changes),
        ("deep_many", fx.deep_tree, fx.deep_tree_many_changes),
    ];
    for (name, old, new) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), &(old, new), |b, pair| {
            b.iter(|| {
                black_box(
                    diff_trees(&fx.repo.odb, Some(&pair.0), Some(&pair.1), "").expect("tree diff"),
                )
            });
        });
    }
    group.finish();
}

fn bench_blob_diff(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("blob_diff");
    let cases = [
        (
            "small_myers",
            &fx.blob_small_old,
            &fx.blob_small_new,
            Algorithm::Myers,
            false,
        ),
        (
            "small_histogram",
            &fx.blob_small_old,
            &fx.blob_small_new,
            Algorithm::Myers,
            true,
        ),
        (
            "10k_lines_myers",
            &fx.blob_large_old,
            &fx.blob_large_new,
            Algorithm::Myers,
            false,
        ),
        (
            "10k_lines_histogram",
            &fx.blob_large_old,
            &fx.blob_large_new,
            Algorithm::Myers,
            true,
        ),
        (
            "pathological_myers",
            &fx.blob_pathological_old,
            &fx.blob_pathological_new,
            Algorithm::Myers,
            false,
        ),
        (
            "pathological_histogram",
            &fx.blob_pathological_old,
            &fx.blob_pathological_new,
            Algorithm::Myers,
            true,
        ),
    ];
    for (name, old, new, algo, histogram) in cases {
        group.throughput(Throughput::Bytes(old.len() as u64 + new.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(old.as_str(), new.as_str(), algo, histogram),
            |b, (old, new, algo, histogram)| {
                b.iter(|| black_box(count_changes_with_algorithm(old, new, *algo, *histogram)));
            },
        );
    }
    group.finish();
}

criterion_group!(
    history,
    bench_s1_rev_list_objects_v1,
    bench_revwalk_topo,
    bench_revwalk_date,
    bench_rev_parse,
    bench_tree_diff,
    bench_blob_diff
);
criterion_main!(history);
