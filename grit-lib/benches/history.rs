//! Criterion benchmarks for revwalk, rev-parse, tree diff, and blob diff.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use grit_lib::progress::{CancelToken, NullProgress};

mod s1;
mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::{line_change_counts, DiffAlgorithm, TreeDiffOptions, UnifiedDiffOptions};
use grit_lib::refs::branch_tracking::TipComparison;

use grit_lib::revwalk::rev_list::{OrderingMode, RevListOptions};

use grit_lib::revwalk::CommitFilterBuilder;
use grit_lib::revwalk::{DescribeOptions, GroupByAuthorOptions, RevWalkListOptions};
use std::sync::{Arc, OnceLock};

use s7::HistoryBenchFixtures;

fn bench_revwalk_path_limited(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("revwalk_path_limited");
    group.throughput(Throughput::Elements(fx.commit_count as u64));
    group.bench_function("walk_f00000", |b| {
        b.iter(|| {
            let opts = RevListOptions::default().with_paths(vec![String::from("f00000.txt")]);
            let result = fx
                .repo
                .revwalk()
                .rev_list(
                    &[String::from("HEAD")],
                    &[],
                    &opts,
                    &mut NullProgress,
                    &CancelToken::never(),
                )
                .expect("rev-list");
            black_box(result.commits.len())
        });
    });
    group.bench_function("count_f00000", |b| {
        b.iter(|| {
            let opts = RevListOptions::default()
                .with_paths(vec![String::from("f00000.txt")])
                .with_count(true)
                .with_use_commit_graph_bloom(true);
            let result = fx
                .repo
                .revwalk()
                .rev_list(
                    &[String::from("HEAD")],
                    &[],
                    &opts,
                    &mut NullProgress,
                    &CancelToken::never(),
                )
                .expect("rev-list");
            black_box(result.commit_count())
        });
    });
    group.bench_function("limit50_f00000", |b| {
        b.iter(|| {
            let opts = RevListOptions::default()
                .with_paths(vec![String::from("f00000.txt")])
                .with_max_count(Some(50))
                .with_use_commit_graph_bloom(true);
            let result = fx
                .repo
                .revwalk()
                .rev_list(
                    &[String::from("HEAD")],
                    &[],
                    &opts,
                    &mut NullProgress,
                    &CancelToken::never(),
                )
                .expect("rev-list");
            black_box(result.commits.len())
        });
    });
    group.finish();
}

fn revwalk_group(c: &mut Criterion, ordering: OrderingMode, label: &str) {
    let mut group = c.benchmark_group(format!("revwalk_{label}"));
    group.throughput(Throughput::Elements(s7::history_commit_count() as u64));

    for (storage, fx) in [
        ("loose", HistoryBenchFixtures::global()),
        ("packed", HistoryBenchFixtures::packed()),
    ] {
        for (graph_label, use_graph) in [("no_commit_graph", false), ("commit_graph", true)] {
            group.bench_with_input(
                BenchmarkId::new(storage, graph_label),
                &use_graph,
                |b, with_graph| {
                    if *with_graph {
                        fx.ensure_commit_graph();
                    } else {
                        fx.remove_commit_graph();
                    }
                    b.iter(|| {
                        let opts = RevListOptions::default()
                            .with_ordering(ordering)
                            .with_use_commit_graph(*with_graph);
                        let result = fx
                            .repo
                            .revwalk()
                            .rev_list(
                                &[String::from("HEAD")],
                                &[],
                                &opts,
                                &mut NullProgress,
                                &CancelToken::never(),
                            )
                            .expect("rev-list");
                        black_box(result.commits.len())
                    });
                },
            );
        }
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

fn bench_revwalk_default(c: &mut Criterion) {
    revwalk_group(c, OrderingMode::Default, "default");
}

fn bench_log_group_by_author(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("log-group-by-author");
    group.throughput(Throughput::Elements(fx.commit_count as u64));
    let walk = RevWalkListOptions::default();
    let mailmap = fx
        .repo
        .revwalk()
        .mailmap_for_grouping(true)
        .expect("mailmap");
    for (label, use_graph) in [("no_commit_graph", false), ("commit_graph", true)] {
        if use_graph {
            fx.ensure_commit_graph();
        }
        group.bench_with_input(BenchmarkId::new("count_only", label), &use_graph, |b, _| {
            b.iter(|| {
                black_box(
                    fx.repo
                        .revwalk()
                        .group_by_author_in_walk(
                            &["HEAD"],
                            &walk,
                            mailmap.as_ref(),
                            GroupByAuthorOptions::default().with_count_only(true),
                        )
                        .expect("group"),
                )
            });
        });
    }
    group.finish();
}

fn bench_revwalk_filter(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let filter = Arc::new(
        CommitFilterBuilder::new()
            .author(".")
            .build()
            .expect("author filter"),
    );
    let mut group = c.benchmark_group("revwalk_filter");
    group.throughput(Throughput::Elements(fx.commit_count as u64));
    fx.ensure_commit_graph();
    group.bench_function("author_over_10k", |b| {
        b.iter(|| {
            let opts = RevListOptions::default().with_commit_filter(Some(filter.clone()));
            let result = fx
                .repo
                .revwalk()
                .rev_list(
                    &[String::from("HEAD")],
                    &[],
                    &opts,
                    &mut NullProgress,
                    &CancelToken::never(),
                )
                .expect("rev-list");
            black_box(result.commits.len())
        });
    });
    group.throughput(Throughput::Elements(50));
    for (storage, fixture) in [("loose", fx), ("packed", HistoryBenchFixtures::packed())] {
        for use_commit_graph in [false, true] {
            let graph = if use_commit_graph {
                "with_graph"
            } else {
                "without_graph"
            };
            group.bench_function(format!("author_limit_50_{storage}_{graph}"), |b| {
                b.iter(|| {
                    let opts = RevListOptions::default()
                        .with_max_count(Some(50))
                        .with_commit_filter(Some(Arc::clone(&filter)))
                        .with_use_commit_graph(use_commit_graph)
                        .with_retain_commit_data(true);
                    let mut result = fixture
                        .repo
                        .revwalk()
                        .rev_list(
                            &[String::from("HEAD")],
                            &[],
                            &opts,
                            &mut NullProgress,
                            &CancelToken::never(),
                        )
                        .expect("rev-list");
                    black_box(
                        fixture
                            .repo
                            .revwalk()
                            .load_commits(
                                &result.commits,
                                &Default::default(),
                                &mut result.commit_data,
                            )
                            .expect("log metadata"),
                    )
                });
            });
        }
    }
    group.throughput(Throughput::Elements(1));
    for (label, pattern) in [("build_dot", "."), ("build_dotted_email", "jane.doe@")] {
        group.bench_function(label, |b| {
            b.iter(|| {
                black_box(
                    CommitFilterBuilder::new()
                        .author(pattern)
                        .grep(pattern)
                        .build()
                        .expect("filter"),
                )
            });
        });
    }
    group.finish();
}

fn bench_revwalk_limit_near_full_history(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let full = fx.commit_count;
    let mut group = c.benchmark_group("revwalk/limit_near_full_history");
    group.throughput(Throughput::Elements(full as u64));
    fx.ensure_commit_graph();
    for (label, max_count) in [
        ("limit_1000", 1000usize),
        ("limit_full", full),
        ("unlimited", usize::MAX),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &max_count,
            |b, &max_count| {
                b.iter(|| {
                    let opts =
                        RevListOptions::default().with_max_count(if max_count == usize::MAX {
                            None
                        } else {
                            Some(max_count)
                        });
                    let result = fx
                        .repo
                        .revwalk()
                        .rev_list(
                            &[String::from("HEAD")],
                            &[],
                            &opts,
                            &mut NullProgress,
                            &CancelToken::never(),
                        )
                        .expect("rev-list");
                    black_box(result.commits.len())
                });
            },
        );
    }
    group.finish();
}

/// `grit log --limit N` walk options: default order, no path reordering, parsed commits kept for
/// rendering. Limits above 4096 used to fall back to a full sort.
fn bench_revwalk_log_limit(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let full = fx.commit_count;
    let mut group = c.benchmark_group("revwalk/log_limit");
    for (label, max_count, retain_commit_data) in [
        ("limit_100", 100usize.min(full), true),
        ("limit_5000", 5000usize.min(full), true),
        ("limit_5000_no_retain", 5000usize.min(full), false),
        ("limit_full", full, true),
    ] {
        group.throughput(Throughput::Elements(max_count as u64));
        group.bench_function(BenchmarkId::from_parameter(label), |b| {
            b.iter(|| {
                let opts = RevListOptions::default()
                    .with_max_count(Some(max_count))
                    .with_retain_commit_data(retain_commit_data);
                let result = fx
                    .repo
                    .revwalk()
                    .rev_list(
                        &[String::from("HEAD")],
                        &[],
                        &opts,
                        &mut NullProgress,
                        &CancelToken::never(),
                    )
                    .expect("rev-list");
                black_box((result.commits.len(), result.commit_data.len()))
            });
        });
    }
    group.finish();
}

fn bench_describe_many_tags(c: &mut Criterion) {
    static TAGGED: OnceLock<()> = OnceLock::new();
    TAGGED.get_or_init(|| {
        let fx = HistoryBenchFixtures::global();
        let opts = RevListOptions::default().with_use_commit_graph(false);
        let commits = fx
            .repo
            .revwalk()
            .rev_list(
                &[String::from("HEAD")],
                &[],
                &opts,
                &mut NullProgress,
                &CancelToken::never(),
            )
            .expect("rev-list")
            .commits;
        for (i, oid) in commits.iter().enumerate().step_by(50) {
            grit_test_support::refs::set_ref(&fx.repo, &format!("refs/tags/describe-t{i}"), oid)
                .expect("tag ref");
        }
    });

    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("revwalk/describe_many_tags");
    for (graph_label, use_graph) in [("no_commit_graph", false), ("commit_graph", true)] {
        group.bench_with_input(
            BenchmarkId::new("head", graph_label),
            &use_graph,
            |b, with_graph| {
                if *with_graph {
                    fx.ensure_commit_graph();
                } else {
                    fx.remove_commit_graph();
                }
                b.iter(|| {
                    black_box(
                        fx.repo
                            .revwalk()
                            .describe(
                                "HEAD",
                                &DescribeOptions::default().with_tags(true),
                                &mut NullProgress,
                                &CancelToken::never(),
                            )
                            .expect("describe"),
                    )
                });
            },
        );
    }
    group.finish();
}

fn bench_log_first_1000_gitgit_like(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mut group = c.benchmark_group("revwalk/log_first_1000_gitgit_like");
    group.throughput(Throughput::Elements(1000));
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
                    let opts = RevListOptions::default()
                        .with_max_count(Some(1000))
                        .with_use_commit_graph(*with_graph);
                    let result = fx
                        .repo
                        .revwalk()
                        .rev_list(
                            &[String::from("HEAD")],
                            &[],
                            &opts,
                            &mut NullProgress,
                            &CancelToken::never(),
                        )
                        .expect("rev-list");
                    black_box(result.commits.len())
                });
            },
        );
    }
    group.finish();
}

fn bench_merge_base_deep(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mid = fx
        .repo
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/heads/bench/b00100")
                .expect("valid ref name"),
        )
        .expect("mid-history branch ref");
    let mut group = c.benchmark_group("revwalk/merge_base_deep");
    for (graph_label, use_graph) in [("no_commit_graph", false), ("commit_graph", true)] {
        group.bench_with_input(
            BenchmarkId::new("merge_base", graph_label),
            &use_graph,
            |b, with_graph| {
                if *with_graph {
                    fx.ensure_commit_graph();
                } else {
                    fx.remove_commit_graph();
                }
                b.iter(|| {
                    black_box(
                        fx.repo
                            .revwalk()
                            .merge_bases(&[fx.head, mid], grit_lib::revwalk::MergeBaseMode::Best)
                            .expect("merge-base")
                            .len(),
                    )
                });
            },
        );
    }
    group.finish();
}

fn bench_ahead_behind_deep(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let mid = fx
        .repo
        .references()
        .resolve(
            grit_lib::refs::store::RefNameRef::new("refs/heads/bench/b00100")
                .expect("valid ref name"),
        )
        .expect("mid-history branch ref");
    let mut group = c.benchmark_group("revwalk/ahead_behind_deep");
    for (graph_label, use_graph) in [("no_commit_graph", false), ("commit_graph", true)] {
        group.bench_with_input(
            BenchmarkId::new("tip_comparison", graph_label),
            &use_graph,
            |b, with_graph| {
                if *with_graph {
                    fx.ensure_commit_graph();
                } else {
                    fx.remove_commit_graph();
                }
                b.iter(|| {
                    let cmp =
                        TipComparison::compute(&fx.repo, fx.head, mid, 10).expect("ahead/behind");
                    black_box((cmp.ahead, cmp.behind))
                });
            },
        );
    }
    group.finish();
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
            b.iter(|| {
                black_box(
                    fx.repo
                        .revwalk()
                        .resolve(spec, &grit_lib::revwalk::ResolveOptions::default())
                        .expect("rev-parse"),
                )
            });
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
                    fx.repo
                        .diff()
                        .tree_to_tree(Some(&pair.0), Some(&pair.1), &TreeDiffOptions::default())
                        .expect("tree diff"),
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
            DiffAlgorithm::Myers,
        ),
        (
            "small_histogram",
            &fx.blob_small_old,
            &fx.blob_small_new,
            DiffAlgorithm::Histogram,
        ),
        (
            "10k_lines_myers",
            &fx.blob_large_old,
            &fx.blob_large_new,
            DiffAlgorithm::Myers,
        ),
        (
            "10k_lines_histogram",
            &fx.blob_large_old,
            &fx.blob_large_new,
            DiffAlgorithm::Histogram,
        ),
        (
            "pathological_myers",
            &fx.blob_pathological_old,
            &fx.blob_pathological_new,
            DiffAlgorithm::Myers,
        ),
        (
            "pathological_histogram",
            &fx.blob_pathological_old,
            &fx.blob_pathological_new,
            DiffAlgorithm::Histogram,
        ),
    ];
    for (name, old, new, algorithm) in cases {
        group.throughput(Throughput::Bytes(old.len() as u64 + new.len() as u64));
        let options = UnifiedDiffOptions::new().with_algorithm(algorithm);
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(old.as_str(), new.as_str()),
            |b, (old, new)| {
                b.iter(|| black_box(line_change_counts(old, new, &options)));
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
    bench_revwalk_default,
    bench_log_group_by_author,
    bench_revwalk_filter,
    bench_revwalk_path_limited,
    bench_describe_many_tags,
    bench_log_first_1000_gitgit_like,
    bench_revwalk_limit_near_full_history,
    bench_revwalk_log_limit,
    bench_merge_base_deep,
    bench_ahead_behind_deep,
    bench_rev_parse,
    bench_tree_diff,
    bench_blob_diff
);
criterion_main!(history);
