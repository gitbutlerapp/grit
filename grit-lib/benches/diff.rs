//! Criterion micro-benchmarks for tree and blob diff hot paths.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod s7;

use grit_lib::objects::FileMode;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::userdiff::matcher_for_driver;
use grit_lib::diff::{
    line_change_counts, unified_diff, DiffAlgorithm, DiffDetail, DiffOptions, HunkExtent,
    RenameDetection, TreeDiffOptions, UnifiedDiffOptions, WhitespaceMode,
};
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::{ObjectId, ObjectKind, TreeEntry};
use grit_lib::prelude::RepositoryOptions;
use grit_lib::repo::{InitOptions, Repository};

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

/// A monorepo-shaped metadata walk, plus reuse of the same child OIDs at
/// hundreds of paths. Cold handles exercise bounded cache eviction; warm
/// handles expose regressions in repeated-subtree reuse.
fn bench_tree_diff_siblings(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff/tree_diff_siblings");
    for repeated in [false, true] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(temp.path(), &InitOptions::default()).expect("init");
        let odb = repo.odb();
        let old_blob = odb.write(ObjectKind::Blob, b"old\n").expect("blob");
        let new_blob = odb.write(ObjectKind::Blob, b"new\n").expect("blob");
        let mut old_root = Vec::new();
        let mut new_root = Vec::new();
        for d in 0..400 {
            let mut entries: Vec<_> = (0..300)
                .map(|f| {
                    TreeEntry::new(
                        format!("f{:03}-{f:04}", if repeated { 0 } else { d }).into_bytes(),
                        FileMode::REGULAR,
                        old_blob,
                    )
                })
                .collect();
            let old = odb
                .write(
                    ObjectKind::Tree,
                    &grit_lib::objects::serialize_tree(&entries),
                )
                .expect("tree");
            for entry in &mut entries[..50] {
                entry.oid = new_blob;
            }
            let new = odb
                .write(
                    ObjectKind::Tree,
                    &grit_lib::objects::serialize_tree(&entries),
                )
                .expect("tree");
            old_root.push(TreeEntry::new(
                format!("d{d:03}").into_bytes(),
                FileMode::TREE,
                old,
            ));
            new_root.push(TreeEntry::new(
                format!("d{d:03}").into_bytes(),
                FileMode::TREE,
                new,
            ));
        }
        let old = odb
            .write(
                ObjectKind::Tree,
                &grit_lib::objects::serialize_tree(&old_root),
            )
            .expect("root");
        let new = odb
            .write(
                ObjectKind::Tree,
                &grit_lib::objects::serialize_tree(&new_root),
            )
            .expect("root");
        let shape = if repeated { "repeated" } else { "distributed" };
        group.throughput(Throughput::Elements(120_000));
        let reopen =
            || Repository::discover(Some(temp.path()), &RepositoryOptions::empty()).expect("open");
        let plain = TreeDiffOptions::default();
        group.bench_function(BenchmarkId::new(shape, "cold"), |b| {
            b.iter_batched(
                reopen,
                |repo| {
                    black_box(
                        repo.diff()
                            .tree_to_tree(Some(&old), Some(&new), &plain)
                            .expect("diff"),
                    )
                },
                criterion::BatchSize::SmallInput,
            );
        });
        group.bench_function(BenchmarkId::new(shape, "warm"), |b| {
            b.iter(|| {
                black_box(
                    repo.diff()
                        .tree_to_tree(Some(&old), Some(&new), &plain)
                        .expect("diff"),
                )
            });
        });
        if !repeated {
            // `grit diff --name-status A B`: default rename detection over modifications only.
            let renames = TreeDiffOptions::default().with_renames(RenameDetection::Similarity(50));
            group.bench_function(BenchmarkId::new(shape, "cold+renames"), |b| {
                b.iter_batched(
                    reopen,
                    |repo| {
                        black_box(
                            repo.diff()
                                .tree_to_tree(Some(&old), Some(&new), &renames)
                                .expect("diff"),
                        )
                    },
                    criterion::BatchSize::SmallInput,
                );
            });
        }
    }
    group.finish();
}

/// Index vs HEAD with a valid cache-tree on a 5k-directory root (the `deep-tree` shape):
/// 50 staged changes, every other subtree skipped through its cache-tree node.
fn bench_index_to_tree_wide_root(c: &mut Criterion) {
    const DIRS: usize = 5000;
    let temp = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(temp.path(), &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let mut index = Index::new();
    for d in 0..DIRS {
        let path = format!("s{d:04x}/a/b/c/leaf.txt");
        let oid = odb.write(ObjectKind::Blob, path.as_bytes()).expect("blob");
        index.add_or_replace(index_entry(&path, oid));
    }
    let head_tree =
        write_tree_update_index(odb, &mut index, "", WriteTreeFlags::default()).expect("tree");
    for d in (0..DIRS).step_by(DIRS / 50) {
        let path = format!("s{d:04x}/a/b/c/leaf.txt");
        index.invalidate_cache_tree_for_path(path.as_bytes());
        let oid = odb.write(ObjectKind::Blob, b"staged").expect("blob");
        index.add_or_replace(index_entry(&path, oid));
    }
    c.bench_function("diff/index_to_tree_wide_root", |b| {
        b.iter(|| {
            black_box(
                repo.diff()
                    .index_to_tree(&index, Some(&head_tree), &TreeDiffOptions::default())
                    .expect("diff"),
            )
        });
    });
}

fn index_entry(path: &str, oid: ObjectId) -> IndexEntry {
    IndexEntry::new(path.as_bytes().to_vec(), oid, FileMode::REGULAR)
}

fn bench_blob_myers_10k_lines(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let old = fx.blob_large_old.as_str();
    let new = fx.blob_large_new.as_str();
    let mut group = c.benchmark_group("diff/blob_myers_10k_lines");
    group.throughput(Throughput::Bytes(old.len() as u64 + new.len() as u64));
    let myers = UnifiedDiffOptions::new();
    let histogram = UnifiedDiffOptions::new().with_algorithm(DiffAlgorithm::Histogram);
    group.bench_function("myers", |b| {
        b.iter(|| black_box(line_change_counts(old, new, &myers)));
    });
    group.bench_function("histogram", |b| {
        b.iter(|| black_box(line_change_counts(old, new, &histogram)));
    });
    group.finish();
}

/// 2,500 small Rust functions (10,000 lines) with an edit every 37 functions, a re-indented
/// body every 53 and an inserted function every 101.
fn code_like_pair() -> (String, String) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..2_500 {
        old.push_str(&format!(
            "fn f{i}(x: u32) -> u32 {{\n    let y = x + {i};\n    y * 2\n}}\n"
        ));
        if i % 101 == 0 {
            new.push_str(&format!("fn added{i}() {{\n    todo!()\n}}\n"));
        }
        let body = if i % 37 == 0 {
            format!("    let y = x - {i};\n")
        } else if i % 53 == 0 {
            format!("        let y = x  +  {i};\n")
        } else {
            format!("    let y = x + {i};\n")
        };
        new.push_str(&format!("fn f{i}(x: u32) -> u32 {{\n{body}    y * 2\n}}\n"));
    }
    (old, new)
}

/// `line_change_counts` per algorithm: the `--stat` / `--numstat` path.
fn bench_text_diff_counts(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let (code_old, code_new) = code_like_pair();
    let mut group = c.benchmark_group("diff/text_count");
    for (name, algorithm) in [
        ("myers", DiffAlgorithm::Myers),
        ("minimal", DiffAlgorithm::Minimal),
        ("patience", DiffAlgorithm::Patience),
        ("histogram", DiffAlgorithm::Histogram),
    ] {
        let options = UnifiedDiffOptions::new().with_algorithm(algorithm);
        group.bench_function(format!("code_{name}"), |b| {
            b.iter(|| black_box(line_change_counts(&code_old, &code_new, &options)));
        });
        if matches!(algorithm, DiffAlgorithm::Myers | DiffAlgorithm::Histogram) {
            let (old, new) = (&fx.blob_pathological_old, &fx.blob_pathological_new);
            group.bench_function(format!("pathological_{name}"), |b| {
                b.iter(|| black_box(line_change_counts(old, new, &options)));
            });
        }
    }
    group.finish();
}

/// `unified_diff` with `a/` / `b/` labels and function names, one row per option.
fn bench_text_diff_render(c: &mut Criterion) {
    let (old, new) = code_like_pair();
    let config = grit_lib::config::ConfigSet::default();
    let rust = matcher_for_driver(&config, "rust")
        .expect("matcher")
        .expect("rust driver");
    let base = UnifiedDiffOptions::new().with_paths("f.rs", "f.rs");
    let cases = [
        ("myers", base.clone()),
        (
            "minimal",
            base.clone().with_algorithm(DiffAlgorithm::Minimal),
        ),
        (
            "patience",
            base.clone().with_algorithm(DiffAlgorithm::Patience),
        ),
        (
            "histogram",
            base.clone().with_algorithm(DiffAlgorithm::Histogram),
        ),
        ("u0", base.clone().with_context_lines(0)),
        ("inter_hunk_6", base.clone().with_inter_hunk_context(6)),
        ("funcname_rust", base.clone().with_funcname(&rust)),
        (
            "ignore_space_change",
            base.clone()
                .with_whitespace(WhitespaceMode::ignore_space_change()),
        ),
        (
            "function_context",
            base.clone().with_extent(HunkExtent::Function),
        ),
        (
            "anchored",
            base.clone().with_anchors(["fn f1250(x: u32) -> u32 {"]),
        ),
    ];
    let mut group = c.benchmark_group("diff/text_render");
    for (name, options) in &cases {
        group.bench_function(*name, |b| {
            b.iter(|| black_box(unified_diff(&old, &new, options)));
        });
    }
    group.finish();
}

fn bench_diff_stat_vs_patch(c: &mut Criterion) {
    let fx = HistoryBenchFixtures::global();
    let entries = fx
        .repo
        .diff()
        .tree_to_tree(
            Some(&fx.wide_tree),
            Some(&fx.wide_tree_few_changes),
            &TreeDiffOptions::default(),
        )
        .expect("tree diff")
        .entries;
    let entry_count = entries.len();
    let opts = DiffOptions::default();
    let mut group = c.benchmark_group("diff/stat_vs_patch");
    group.bench_function("stat", |b| {
        b.iter(|| {
            black_box(
                fx.repo
                    .diff()
                    .files_for_entries(
                        entries[..entry_count.min(entries.len())].to_vec(),
                        DiffDetail::Stat,
                        &opts,
                    )
                    .expect("stat"),
            )
        });
    });
    group.bench_function("patch", |b| {
        b.iter(|| {
            black_box(
                fx.repo
                    .diff()
                    .files_for_entries(
                        entries[..entry_count.min(entries.len())].to_vec(),
                        DiffDetail::Patch,
                        &opts,
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
    bench_tree_diff_siblings,
    bench_index_to_tree_wide_root,
    bench_blob_myers_10k_lines,
    bench_text_diff_counts,
    bench_text_diff_render,
    bench_diff_stat_vs_patch
);
criterion_main!(diff_benches);
