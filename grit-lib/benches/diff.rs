//! Criterion micro-benchmarks for tree and blob diff hot paths.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s7;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    build_file_diffs, count_changes_git_histogram, count_changes_with_algorithm,
    diff_index_to_tree, diff_trees, DiffDetail, DiffOptions,
};
use grit_lib::index::write_tree::{build_cache_tree_from_index, write_tree_from_index};
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::parallel::ThreadPolicy;
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

/// Index vs HEAD with a valid cache-tree on a 5k-directory root (the `deep-tree` shape):
/// 50 staged changes, every other subtree skipped through its cache-tree node.
fn bench_index_to_tree_wide_root(c: &mut Criterion) {
    const DIRS: usize = 5000;
    let temp = tempfile::tempdir().expect("tempdir");
    let odb = Odb::new(temp.path());
    let mut index = Index::new();
    for d in 0..DIRS {
        let path = format!("s{d:04x}/a/b/c/leaf.txt");
        let oid = odb.write(ObjectKind::Blob, path.as_bytes()).expect("blob");
        index.add_or_replace(index_entry(&path, oid));
    }
    let head_tree = write_tree_from_index(&odb, &index, "").expect("tree");
    index.set_cache_tree(build_cache_tree_from_index(&odb, &index).expect("cache-tree"));
    for d in (0..DIRS).step_by(DIRS / 50) {
        let path = format!("s{d:04x}/a/b/c/leaf.txt");
        index.invalidate_cache_tree_for_path(path.as_bytes());
        let oid = odb.write(ObjectKind::Blob, b"staged").expect("blob");
        index.add_or_replace(index_entry(&path, oid));
    }
    c.bench_function("diff/index_to_tree_wide_root", |b| {
        b.iter(|| {
            black_box(diff_index_to_tree(&odb, &index, Some(&head_tree), false).expect("diff"))
        });
    });
}

fn index_entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: 0o100644,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
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

fn bench_diff_stat_vs_patch(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let entries = diff_trees(
        &fx.repo.odb(),
        Some(&fx.wide_tree),
        Some(&fx.wide_tree_few_changes),
        "",
    )
    .expect("tree diff");
    let entry_count = entries.len();
    let config = ConfigSet::new();
    let policy = ThreadPolicy::default();
    let opts = DiffOptions::default();
    let mut group = c.benchmark_group("diff/stat_vs_patch");
    group.bench_function("stat", |b| {
        b.iter(|| {
            black_box(
                build_file_diffs(
                    fx.repo.odb(),
                    entries[..entry_count.min(entries.len())].to_vec(),
                    DiffDetail::Stat,
                    opts.clone(),
                    &config,
                    &policy,
                )
                .expect("stat"),
            )
        });
    });
    group.bench_function("patch", |b| {
        b.iter(|| {
            black_box(
                build_file_diffs(
                    fx.repo.odb(),
                    entries[..entry_count.min(entries.len())].to_vec(),
                    DiffDetail::Patch,
                    opts.clone(),
                    &config,
                    &policy,
                )
                .expect("patch"),
            )
        });
    });
    group.finish();
}

criterion_group!(
    diff_benches,
    bench_tree_diff_wide,
    bench_index_to_tree_wide_root,
    bench_blob_myers_10k_lines,
    bench_diff_stat_vs_patch
);
criterion_main!(diff_benches);
