//! Wide clean content merge hot path from issue #2036 (both sides edit every file).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use criterion::{criterion_group, criterion_main, Criterion};
use grit_lib::index::MODE_REGULAR;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use std::hint::black_box;
use tempfile::TempDir;

/// Both sides edit each of 20k small text files, as in the daily wide-content scenario.
fn content_merge_fixture() -> (TempDir, Repository, [ObjectId; 3]) {
    use grit_lib::objects::{serialize_tree, TreeEntry};

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::new()).expect("init");
    let mut roots: [Vec<TreeEntry>; 3] = std::array::from_fn(|_| Vec::new());
    for d in 0..200 {
        let mut files: [Vec<TreeEntry>; 3] = std::array::from_fn(|_| Vec::new());
        for f in 0..100 {
            let body: String = (0..20).map(|j| format!("line {j} of {d}/{f}\n")).collect();
            for (entries, content) in files.iter_mut().zip([
                body.clone(),
                format!("main head\n{body}"),
                format!("{body}topic tail\n"),
            ]) {
                entries.push(TreeEntry {
                    mode: MODE_REGULAR,
                    name: format!("f{f:04}.txt").into_bytes(),
                    oid: repo
                        .odb()
                        .write(ObjectKind::Blob, content.as_bytes())
                        .expect("blob"),
                });
            }
        }
        for (root, entries) in roots.iter_mut().zip(files) {
            root.push(TreeEntry {
                mode: 0o040000,
                name: format!("d{d:04}").into_bytes(),
                oid: repo
                    .odb()
                    .write(ObjectKind::Tree, &serialize_tree(&entries))
                    .expect("subtree"),
            });
        }
    }
    let trees = roots.map(|entries| {
        repo.odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))
            .expect("tree")
    });
    (dir, repo, trees)
}

fn bench_content_merge_20k(c: &mut Criterion) {
    let (_dir, repo, [base, ours, theirs]) = content_merge_fixture();
    let mut group = c.benchmark_group("merge_trees_three_way");
    group.sample_size(10);
    group.bench_function("content_both_sides_20k", |b| {
        b.iter(|| {
            black_box(
                grit_lib::diff::merge_trees::merge_trees_three_way(
                    &repo,
                    base,
                    ours,
                    theirs,
                    grit_lib::diff::merge_file::MergeFavor::default(),
                    grit_lib::diff::merge_trees::WhitespaceMergeOptions::default(),
                    None,
                    grit_lib::diff::merge_trees::TreeMergeConflictPresentation::default(),
                )
                .expect("content merge"),
            );
        });
    });
    group.finish();
}

criterion_group!(benches, bench_content_merge_20k);
criterion_main!(benches);
