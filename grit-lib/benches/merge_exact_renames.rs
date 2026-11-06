//! Three-way merge throughput when one side performs many exact renames (issue #2508).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{
    merge_trees_three_way, DirectoryRenameMode, TreeMergeConflictPresentation,
    WhitespaceMergeOptions,
};
use grit_lib::index::MODE_REGULAR;
use grit_lib::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

const MODE_DIR: u32 = 0o040000;

fn file_entries(files: usize, blobs: &[ObjectId]) -> Vec<TreeEntry> {
    (0..files)
        .map(|f| TreeEntry {
            mode: MODE_REGULAR,
            name: format!("file{f:04}.c").into_bytes(),
            oid: blobs[f],
        })
        .collect()
}

fn write_tree(odb: &grit_lib::objects::odb::Odb, entries: &[TreeEntry]) -> ObjectId {
    odb.write(ObjectKind::Tree, &serialize_tree(entries))
        .expect("tree")
}

fn driver_subtree(
    odb: &grit_lib::objects::odb::Odb,
    subs: usize,
    files: usize,
    blobs: &[ObjectId],
) -> ObjectId {
    let impl_oid = write_tree(odb, &file_entries(files, blobs));
    let sub_entries: Vec<TreeEntry> = (0..subs)
        .map(|s| TreeEntry {
            mode: MODE_DIR,
            name: format!("sub{s:03}").into_bytes(),
            oid: write_tree(
                odb,
                &[TreeEntry {
                    mode: MODE_DIR,
                    name: b"impl".to_vec(),
                    oid: impl_oid,
                }],
            ),
        })
        .collect();
    write_tree(odb, &sub_entries)
}

fn exact_rename_merge_fixture(
    total_dirs: usize,
    moved_dirs: usize,
    subs: usize,
    files: usize,
) -> (TempDir, Repository, [ObjectId; 3]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::new()).expect("init");
    let odb = repo.odb();

    let blobs: Vec<ObjectId> = (0..files)
        .map(|f| {
            odb.write(ObjectKind::Blob, format!("payload {f}\n").as_bytes())
                .expect("blob")
        })
        .collect();

    let driver_oids: Vec<ObjectId> = (0..total_dirs)
        .map(|_| driver_subtree(odb, subs, files, &blobs))
        .collect();

    let base_roots: Vec<TreeEntry> = (0..total_dirs)
        .map(|d| TreeEntry {
            mode: MODE_DIR,
            name: format!("drivers{d:03}").into_bytes(),
            oid: driver_oids[d],
        })
        .collect();
    let base = write_tree(odb, &base_roots);

    let edited = odb
        .write(ObjectKind::Blob, b"payload 0\nmain edit\n")
        .expect("edited");

    let mut ours_driver_oids = driver_oids.clone();
    let ours_driver0 = driver_subtree(odb, subs, files, &{
        let mut b = blobs.clone();
        b[0] = edited;
        b
    });
    ours_driver_oids[moved_dirs] = ours_driver0;

    let ours_roots: Vec<TreeEntry> = (0..total_dirs)
        .map(|d| TreeEntry {
            mode: MODE_DIR,
            name: format!("drivers{d:03}").into_bytes(),
            oid: ours_driver_oids[d],
        })
        .collect();
    let ours = write_tree(odb, &ours_roots);

    let arch: Vec<TreeEntry> = (0..moved_dirs)
        .map(|d| TreeEntry {
            mode: MODE_DIR,
            name: format!("drivers{d:03}").into_bytes(),
            oid: driver_oids[d],
        })
        .collect();
    let arch_oid = write_tree(odb, &arch);

    let mut ren_roots: Vec<TreeEntry> = (moved_dirs..total_dirs)
        .map(|d| TreeEntry {
            mode: MODE_DIR,
            name: format!("drivers{d:03}").into_bytes(),
            oid: driver_oids[d],
        })
        .collect();
    ren_roots.push(TreeEntry {
        mode: MODE_DIR,
        name: b"arch".to_vec(),
        oid: arch_oid,
    });
    let theirs = write_tree(odb, &ren_roots);

    (dir, repo, [base, ours, theirs])
}

fn bench_exact_renames(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge_exact_renames");
    group.sample_size(10);
    for (moved_dirs, subs, files) in [(10, 2, 50), (20, 2, 50)] {
        let renames = moved_dirs * subs * files;
        let total_dirs = moved_dirs * 2;
        let (_dir, repo, [base, ours, theirs]) =
            exact_rename_merge_fixture(total_dirs, moved_dirs, subs, files);
        group.throughput(Throughput::Elements(renames as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{renames}_renames")),
            &renames,
            |b, _| {
                b.iter(|| {
                    black_box(
                        merge_trees_three_way(
                            &repo,
                            base,
                            ours,
                            theirs,
                            MergeFavor::default(),
                            WhitespaceMergeOptions::default(),
                            None,
                            TreeMergeConflictPresentation::default(),
                            DirectoryRenameMode::default(),
                        )
                        .expect("merge"),
                    );
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_exact_renames);
criterion_main!(benches);
