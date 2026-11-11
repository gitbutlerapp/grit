//! Disjoint repeated basenames must skip inexact rename work above renameLimit (issue #2436).

use grit_lib::diff::{zero_oid, DiffEntry, DiffStatus, RenameDetection, TreeDiffOptions};
use grit_lib::objects::FileMode;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::forced_parallel_policy;
use tempfile::TempDir;

fn disjoint_f_vs_g_entries(n: usize) -> (TempDir, Repository, Vec<DiffEntry>) {
    let root = TempDir::new().expect("tempdir");
    let (repo, _) = Repository::init(root.path(), &InitOptions::default()).expect("init");
    let odb = repo.odb();
    let mut entries = Vec::with_capacity(n * 2);
    for i in 0..n {
        let old_oid = odb
            .write(ObjectKind::Blob, format!("old payload {i}\n").as_bytes())
            .expect("old blob");
        let new_oid = odb
            .write(ObjectKind::Blob, format!("new payload {i}\n").as_bytes())
            .expect("new blob");
        entries.push(DiffEntry::new(
            DiffStatus::Deleted,
            Some(format!("a/d{}/f{}.txt", i / 50, i % 50)),
            None,
            FileMode::REGULAR,
            FileMode::ABSENT,
            old_oid,
            zero_oid(),
        ));
        entries.push(DiffEntry::new(
            DiffStatus::Added,
            None,
            Some(format!("b/d{}/g{}.txt", i / 50, i % 50)),
            FileMode::ABSENT,
            FileMode::REGULAR,
            zero_oid(),
            new_oid,
        ));
    }
    (root, repo, entries)
}

#[test]
fn disjoint_repeated_basenames_skip_inexact_rename_detection() {
    const N: usize = 2000;
    let (_root, repo, entries) = disjoint_f_vs_g_entries(N);
    let options = TreeDiffOptions::default()
        .with_renames(RenameDetection::Similarity(50))
        .with_threads(Some(forced_parallel_policy()));
    let result = repo
        .diff()
        .detect_renames(entries, None, &options)
        .expect("detect renames");

    assert!(
        result.inexact_rename_skipped.is_some(),
        "expected inexact rename detection to be skipped above renameLimit"
    );
    let renames = result
        .entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .count();
    assert_eq!(renames, 0, "disjoint basenames must not pair as renames");
}
