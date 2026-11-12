//! Tree↔worktree diff must honor `core.autocrlf` and `core.filemode` (issue #947).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{mode_from_metadata, TreeDiffOptions};
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{entry_from_stat, Index, MODE_EXECUTABLE, MODE_REGULAR};
use grit_lib::objects::FileMode;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Open a repository at `wt` whose `.git` holds only `config` (plus `HEAD`, `objects`, `refs`).
fn open_repo_with_config(wt: &Path, config: &str) -> Repository {
    let git_dir = wt.join(".git");
    fs::create_dir_all(git_dir.join("objects")).expect("objects");
    fs::create_dir_all(git_dir.join("refs")).expect("refs");
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    fs::write(git_dir.join("config"), config).expect("config");
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open")
}

fn repo_with_autocrlf_filemode_false(
    root: &TempDir,
) -> (Repository, Index, grit_lib::objects::ObjectId) {
    let wt = root.path();
    let repo = open_repo_with_config(
        wt,
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tautocrlf = true\n\tfilemode = false\n",
    );

    // LF blob in Git (as after `git add` with autocrlf).
    let odb = repo.odb();
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

    let tree_oid = write_tree_from_index(odb, &index, "").expect("tree");
    (repo, index, tree_oid)
}

#[test]
fn diff_tree_to_worktree_empty_with_autocrlf_and_filemode_false() {
    let root = TempDir::new().expect("tempdir");
    let (repo, index, tree_oid) = repo_with_autocrlf_filemode_false(&root);

    let diff = repo
        .diff()
        .tree_to_worktree(
            Some(&tree_oid),
            &index,
            &TreeDiffOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("diff")
        .entries;

    assert!(
        diff.is_empty(),
        "tree↔worktree diff must ignore CRLF smudge and mode when filemode=false: {diff:?}"
    );
}

#[test]
fn diff_tree_to_worktree_empty_when_stat_cache_stale() {
    let root = TempDir::new().expect("tempdir");
    let (repo, mut index, tree_oid) = repo_with_autocrlf_filemode_false(&root);
    // Force a full content hash pass (stat cache does not match worktree).
    for ie in index.entries_mut() {
        ie.mtime_sec = 0;
        ie.size = 0;
    }

    let diff = repo
        .diff()
        .tree_to_worktree(
            Some(&tree_oid),
            &index,
            &TreeDiffOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("diff")
        .entries;

    assert!(
        diff.is_empty(),
        "CRLF normalization must apply even when stat cache is stale: {diff:?}"
    );
}

#[test]
fn explicit_index_chmod_reported_when_filemode_false() {
    let root = TempDir::new().expect("tempdir");
    let wt = root.path();
    let repo = open_repo_with_config(
        wt,
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tfilemode = false\n",
    );

    fs::write(wt.join("run.sh"), b"#!/bin/sh\n").expect("file");
    let odb = repo.odb();
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
    let tree_oid = write_tree_from_index(odb, &index, "").expect("tree");

    // Staged `git update-index --chmod=+x` while HEAD/worktree stay 100644.
    index.entries_mut()[0].mode = MODE_EXECUTABLE;

    let diff = repo
        .diff()
        .tree_to_worktree(
            Some(&tree_oid),
            &index,
            &TreeDiffOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("diff")
        .entries;

    assert_eq!(
        diff.len(),
        1,
        "explicit index chmod must appear in tree diff even when core.filemode=false: {diff:?}"
    );
    assert_eq!(diff[0].old_mode, FileMode::REGULAR);
    assert_eq!(diff[0].new_mode, FileMode::EXECUTABLE);
    assert_eq!(diff[0].old_oid, diff[0].new_oid);
}

#[test]
fn diff_index_to_worktree_stays_empty_in_same_fixture() {
    use grit_lib::diff::SubmoduleIgnoreMode;

    let root = TempDir::new().expect("tempdir");
    let (repo, mut index, _tree) = repo_with_autocrlf_filemode_false(&root);

    let options = TreeDiffOptions::default().with_submodules(SubmoduleIgnoreMode::Dirty);
    let diff = repo
        .diff()
        .index_to_worktree(
            &mut index,
            &options,
            &mut grit_lib::progress::NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("diff")
        .entries;
    assert!(
        diff.is_empty(),
        "index↔worktree should already be clean in this fixture: {diff:?}"
    );
}
