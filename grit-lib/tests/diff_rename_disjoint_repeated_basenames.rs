//! Disjoint repeated basenames must skip inexact rename work above renameLimit (issue #2436).

use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    detect_renames_with_policy, zero_oid, DetectRenamesOptions, DiffEntry, DiffStatus,
};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::FileMode;
use grit_lib::objects::ObjectKind;
use grit_test_support::forced_parallel_policy;
use tempfile::TempDir;

fn disjoint_f_vs_g_entries(n: usize) -> (TempDir, Odb, Vec<DiffEntry>) {
    let root = TempDir::new().expect("tempdir");
    let git_dir = root.path().join(".git");
    std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
    let odb = Odb::new(&git_dir.join("objects"));
    let mut entries = Vec::with_capacity(n * 2);
    for i in 0..n {
        let old_oid = odb
            .write(ObjectKind::Blob, format!("old payload {i}\n").as_bytes())
            .expect("old blob");
        let new_oid = odb
            .write(ObjectKind::Blob, format!("new payload {i}\n").as_bytes())
            .expect("new blob");
        entries.push(DiffEntry {
            status: DiffStatus::Deleted,
            old_path: Some(format!("a/d{}/f{}.txt", i / 50, i % 50)),
            new_path: None,
            old_mode: FileMode::REGULAR,
            new_mode: FileMode::ABSENT,
            old_oid,
            new_oid: zero_oid(),
            score: None,
        });
        entries.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(format!("b/d{}/g{}.txt", i / 50, i % 50)),
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::REGULAR,
            old_oid: zero_oid(),
            new_oid,
            score: None,
        });
    }
    (root, odb, entries)
}

#[test]
fn disjoint_repeated_basenames_skip_inexact_rename_detection() {
    const N: usize = 2000;
    let (_root, odb, entries) = disjoint_f_vs_g_entries(N);
    let config = ConfigSet::new();
    let policy = forced_parallel_policy();
    let result = detect_renames_with_policy(
        &odb,
        None,
        entries,
        50,
        &config,
        &policy,
        None,
        DetectRenamesOptions::default(),
    )
    .expect("detect renames");

    assert!(
        result.inexact_skipped.is_some(),
        "expected inexact rename detection to be skipped above renameLimit"
    );
    let renames = result
        .entries
        .iter()
        .filter(|e| e.status == DiffStatus::Renamed)
        .count();
    assert_eq!(renames, 0, "disjoint basenames must not pair as renames");
}
