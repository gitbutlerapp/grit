//! Criterion micro-benchmarks for index I/O, config load, ignore, and attributes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod worktree_fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::config::environment::Environment;
use grit_lib::config::ConfigSet;
use grit_lib::index::{Index, IndexParseContext};
use grit_lib::objects::HashAlgo;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::status::{
    collect_untracked_and_ignored, status, IgnoredMode, StatusOptions,
};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::worktree::attributes::collect_attrs_for_path;
use grit_lib::worktree::ignore::IgnoreMatcher;

use worktree_fixture::WorktreeBenchFixtures;

fn ieot_bench_config() -> ConfigSet {
    let dir = tempfile::tempdir().expect("ieot bench config dir");
    std::fs::write(
        dir.path().join("config"),
        "[index]\n\tthreads = 4\n\trecordOffsetTable = true\n\trecordEndOfIndexEntries = true\n",
    )
    .expect("config");
    ConfigSet::load(&Environment::empty(), Some(dir.path()), true).expect("load config")
}

fn bench_index_read(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let ieot_cfg = ieot_bench_config();
    let ctx_serial = IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(1)));
    let ctx_parallel = IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(4)));
    let mut group = c.benchmark_group("index_read");
    for (label, path) in [
        ("v2/10000", &fx.index_v2_10k_path),
        ("v2/100000", &fx.index_v2_100k_path),
        ("v4/10000", &fx.index_v4_10k_path),
        ("v4/100000", &fx.index_v4_100k_path),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(label), path, |b, index_path| {
            b.iter(|| black_box(Index::load(index_path).expect("load index")));
        });
    }
    for (label, path) in [
        ("ieot/v4/100000/t1", &fx.index_v4_100k_ieot_path),
        ("ieot/v4/100000/t4", &fx.index_v4_100k_ieot_path),
    ] {
        let ctx = if label.ends_with("t4") {
            ctx_parallel
        } else {
            ctx_serial
        };
        let data = std::fs::read(path).expect("read ieot index");
        group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, bytes| {
            b.iter(|| {
                black_box(
                    Index::parse_with_algo_and_config(bytes, HashAlgo::Sha1, Some(&ieot_cfg), ctx)
                        .expect("parse ieot index"),
                )
            });
        });
    }
    if let Some(path) = &fx.index_v4_1m_ieot_path {
        for threads in [1_usize, 4] {
            let ctx =
                IndexParseContext::from_policy(ThreadPolicy::with_threads_override(Some(threads)));
            let data = std::fs::read(path).expect("read 1m ieot index");
            let label = format!("ieot/v4/1000000/t{threads}");
            group.bench_with_input(BenchmarkId::from_parameter(label), &data, |b, bytes| {
                b.iter(|| {
                    black_box(
                        Index::parse_with_algo_and_config(
                            bytes,
                            HashAlgo::Sha1,
                            Some(&ieot_cfg),
                            ctx,
                        )
                        .expect("parse 1m ieot index"),
                    )
                });
            });
        }
    }
    group.finish();
}

fn bench_index_write(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let mut group = c.benchmark_group("index_write");
    for (label, index) in [
        ("v2/10000", &fx.index_v2_10k),
        ("v4/10000", &fx.index_v4_10k),
    ] {
        group.throughput(Throughput::Elements(index.entries().len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), index, |b, idx| {
            b.iter(|| {
                let mut idx = idx.clone();
                idx.write(&fx.index_write_scratch)
                    .expect("write index bench");
            });
        });
    }
    group.finish();
}

fn bench_index_write_100k(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let mut group = c.benchmark_group("index/write_100k");
    for (label, index) in [("v2", &fx.index_v2_100k), ("v4", &fx.index_v4_100k)] {
        group.throughput(Throughput::Elements(100_000));
        group.bench_with_input(BenchmarkId::from_parameter(label), index, |b, idx| {
            b.iter(|| {
                let mut idx = idx.clone();
                idx.write(&fx.index_write_scratch)
                    .expect("write index bench");
            });
        });
    }
    group.finish();
}

fn bench_add_stage_all_100k(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let repo = &fx.scan_repo_100k;
    let wt = repo.work_tree().expect("work tree");
    let opts = StageOptions::default();
    let mut progress = NullProgress;
    let mut group = c.benchmark_group("add/stage_all_100k");
    group.throughput(Throughput::Elements(100_000));
    group.bench_function("stat_clean", |b| {
        let _ = wt;
        b.iter(|| black_box(stage(repo, &opts, &mut progress).expect("stage all")));
    });
    group.finish();
}

fn bench_config_load(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    c.bench_function("config_load_cascade", |b| {
        b.iter(|| {
            fx.bump_config_stamp();
            black_box(fx.load_config_cascade());
        });
    });
}

fn bench_ignore_match(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let paths = &fx.ignore_paths[..10_000];
    let mut matcher = IgnoreMatcher::from_repository(&fx.ignore_repo).expect("ignore matcher");
    let mut group = c.benchmark_group("ignore/match_10k_paths");
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("check_path", |b| {
        b.iter(|| {
            for path in paths {
                black_box(
                    matcher
                        .check_path(&fx.ignore_repo, None, path, false)
                        .expect("check ignore"),
                );
            }
        });
    });
    group.finish();
}

fn bench_untracked_walk(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let repo = &fx.untracked_walk_repo;
    let work_tree = repo.work_tree().expect("work tree");
    let mut group = c.benchmark_group("status/untracked_walk_100k");
    group.bench_function("collect_untracked", |b| {
        b.iter(|| {
            let mut index = fx.untracked_walk_index.clone();
            black_box(
                collect_untracked_and_ignored(
                    repo,
                    &mut index,
                    work_tree,
                    IgnoredMode::No,
                    false,
                    &[],
                )
                .expect("collect untracked"),
            );
        });
    });
    group.finish();
}

fn bench_attributes_match(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let rules = fx.attr_stack.rules.as_slice();
    let macros = &fx.attr_stack.macros;
    let paths = &fx.attr_paths;
    c.bench_function("gitattributes_match_100k_paths", |b| {
        b.iter(|| {
            for path in paths {
                black_box(collect_attrs_for_path(rules, macros, path, false));
            }
        });
    });
}

fn bench_status_with_nested_gitattributes_l(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let opts = StatusOptions::default();
    let mut group = c.benchmark_group("status_with_nested_gitattributes_L");
    group.bench_function("plain_no_nested_attrs", |b| {
        b.iter(|| {
            black_box(
                status(
                    &fx.status_plain_l_repo,
                    &opts,
                    &mut NullProgress,
                    &NeverCancel,
                )
                .expect("status plain L"),
            );
        });
    });
    group.bench_function("nested_gitattributes", |b| {
        b.iter(|| {
            black_box(
                status(
                    &fx.status_nested_attr_repo,
                    &opts,
                    &mut NullProgress,
                    &NeverCancel,
                )
                .expect("status nested attrs L"),
            );
        });
    });
    group.finish();
}

criterion_group!(
    worktree,
    bench_index_read,
    bench_index_write,
    bench_index_write_100k,
    bench_add_stage_all_100k,
    bench_config_load,
    bench_ignore_match,
    bench_untracked_walk,
    bench_attributes_match,
    bench_status_with_nested_gitattributes_l
);
criterion_main!(worktree);
