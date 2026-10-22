//! Tree↔worktree diff must honor `core.autocrlf` and `core.filemode` (issue #947).

use grit_lib::config::ConfigSet;
use grit_lib::diff::{diff_tree_to_worktree_with_git_dir, mode_from_metadata};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{entry_from_stat, Index, MODE_EXECUTABLE, MODE_REGULAR};
use grit_lib::objects::odb::Odb;
use grit_lib::objects::ObjectKind;
use std::fs;
use tempfile::TempDir;

fn repo_with_autocrlf_filemode_false(root: &TempDir) -> (Odb, Index, grit_lib::objects::ObjectId) {
    let wt = root.path();
    let git_dir = wt.join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::create_dir_all(git_dir.join("refs")).expect("refs");
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n\tfilemode = false\n",
    )
    .expect("config");

    // LF blob in Git (as after `git add` with autocrlf).
    let odb = Odb::new(&git_dir.join("objects"));
    let oid = odb
        .write(ObjectKind::Blob, b"one\ntwo\nthree\n")
        .expect("blob");

    // Worktree file checked out with CRLF (Git smudge on Windows).
    fs::write(wt.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").expect("worktree");

    let meta = fs::symlink_metadata(wt.join("a.txt")).expect("stat");
    let mut entry = entry_from_stat(&wt.join("a.txt"), b"a.txt", oid, mode_from_metadata(&meta))
        .expect("entry");
    // Index/tree still record executable bit from `git update-index --chmod=+x`.
    entry.mode = MODE_EXECUTABLE;
    let mut index = Index::new();
    index.entries_mut().push(entry);

    let tree_oid = write_tree_from_index(&odb, &index, "").expect("tree");
    (odb, index, tree_oid)
}

#[test]
fn diff_tree_to_worktree_empty_with_autocrlf_and_filemode_false() {
    let root = TempDir::new().expect("tempdir");
    let (odb, index, tree_oid) = repo_with_autocrlf_filemode_false(&root);
    let cfg = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(&root.path().join(".git")),
        true,
    )
    .expect("config");

    let diff = diff_tree_to_worktree_with_git_dir(
        &odb,
        Some(&tree_oid),
        root.path(),
        &root.path().join(".git"),
        &index,
        Some(&cfg),
    )
    .expect("diff");

    assert!(
        diff.is_empty(),
        "tree↔worktree diff must ignore CRLF smudge and mode when filemode=false: {diff:?}"
    );
}

#[test]
fn diff_tree_to_worktree_empty_when_stat_cache_stale() {
    let root = TempDir::new().expect("tempdir");
    let (odb, mut index, tree_oid) = repo_with_autocrlf_filemode_false(&root);
    // Force a full content hash pass (stat cache does not match worktree).
    for ie in index.entries_mut() {
        ie.mtime_sec = 0;
        ie.size = 0;
    }
    let cfg = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(&root.path().join(".git")),
        true,
    )
    .expect("config");

    let diff = diff_tree_to_worktree_with_git_dir(
        &odb,
        Some(&tree_oid),
        root.path(),
        &root.path().join(".git"),
        &index,
        Some(&cfg),
    )
    .expect("diff");

    assert!(
        diff.is_empty(),
        "CRLF normalization must apply even when stat cache is stale: {diff:?}"
    );
}

#[test]
fn explicit_index_chmod_reported_when_filemode_false() {
    let root = TempDir::new().expect("tempdir");
    let wt = root.path();
    let git_dir = wt.join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfilemode = false\n",
    )
    .expect("config");

    fs::write(wt.join("run.sh"), b"#!/bin/sh\n").expect("file");
    let odb = Odb::new(&git_dir.join("objects"));
    let oid = odb.write(ObjectKind::Blob, b"#!/bin/sh\n").expect("blob");

    let meta = fs::symlink_metadata(wt.join("run.sh")).expect("stat");
    let wt_mode = mode_from_metadata(&meta);
    assert_eq!(
        wt_mode, MODE_REGULAR,
        "fixture expects non-executable worktree file (100644 HEAD/worktree)"
    );

    let entry = entry_from_stat(&wt.join("run.sh"), b"run.sh", oid, MODE_REGULAR).expect("entry");
    let mut index = Index::new();
    index.entries_mut().push(entry);
    let tree_oid = write_tree_from_index(&odb, &index, "").expect("tree");

    // Staged `git update-index --chmod=+x` while HEAD/worktree stay 100644.
    index.entries_mut()[0].mode = MODE_EXECUTABLE;

    let cfg = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(&git_dir),
        true,
    )
    .expect("config");

    let diff =
        diff_tree_to_worktree_with_git_dir(&odb, Some(&tree_oid), wt, &git_dir, &index, Some(&cfg))
            .expect("diff");

    assert_eq!(
        diff.len(),
        1,
        "explicit index chmod must appear in tree diff even when core.filemode=false: {diff:?}"
    );
    assert_eq!(diff[0].old_mode, "100644");
    assert_eq!(diff[0].new_mode, "100755");
    assert_eq!(diff[0].old_oid, diff[0].new_oid);
}

#[test]
fn diff_index_to_worktree_stays_empty_in_same_fixture() {
    use grit_lib::diff::diff_index_to_worktree;

    let root = TempDir::new().expect("tempdir");
    let (odb, mut index, _tree) = repo_with_autocrlf_filemode_false(&root);

    let diff = diff_index_to_worktree(&odb, &mut index, root.path(), false, true).expect("diff");
    assert!(
        diff.is_empty(),
        "index↔worktree should already be clean in this fixture: {diff:?}"
    );
}
