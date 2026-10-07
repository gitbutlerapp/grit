//! Criterion micro-benchmarks for index I/O, config load, ignore, and attributes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod worktree_fixture;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::attributes::collect_attrs_for_path;
use grit_lib::ignore::IgnoreMatcher;
use grit_lib::index::Index;

use worktree_fixture::WorktreeBenchFixtures;

fn bench_index_read(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
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
    group.finish();
}

fn bench_index_write(c: &mut Criterion) {
    let fx = WorktreeBenchFixtures::global();
    let mut group = c.benchmark_group("index_write");
    for (label, index) in [
        ("v2/10000", &fx.index_v2_10k),
        ("v2/100000", &fx.index_v2_100k),
        ("v4/10000", &fx.index_v4_10k),
        ("v4/100000", &fx.index_v4_100k),
    ] {
        group.throughput(Throughput::Elements(index.entries.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), index, |b, idx| {
            b.iter(|| {
                idx.write(&fx.index_write_scratch)
                    .expect("write index bench");
            });
        });
    }
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
    let paths = &fx.ignore_paths;
    let mut matcher = IgnoreMatcher::from_repository(&fx.ignore_repo).expect("ignore matcher");
    c.bench_function("ignore_match_100k_paths", |b| {
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

criterion_group!(
    worktree,
    bench_index_read,
    bench_index_write,
    bench_config_load,
    bench_ignore_match,
    bench_attributes_match
);
criterion_main!(worktree);
