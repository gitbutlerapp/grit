//! Opportunistic status index writes must not overwrite a concurrent commit (issue #2317).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::status::{
    status, BeforeOpportunisticIndexWriteHook, StatusOptions, UntrackedMode,
};
use grit_lib::progress::{NeverCancel, NullProgress};

use crate::support::{assert_git_fsck_strict, git, open_grit_repo};

fn git_diff_cached_names(repo_root: &Path) -> String {
    let root = repo_root.to_str().expect("utf-8 path");
    let out = Command::new("git")
        .args(["-C", root, "diff", "--cached", "--name-only"])
        .output()
        .expect("git diff --cached");
    assert!(out.status.success(), "git diff --cached failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "a@b.c"]);
    git(dir, &["config", "user.name", "A"]);
    fs::write(dir.join("file.txt"), "base\n").expect("write file");
    git(dir, &["add", "file.txt"]);
    git(dir, &["commit", "-qm", "init"]);
}

#[test]
fn status_skips_stale_opportunistic_write_after_concurrent_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    fs::write(dir.join("file.txt"), "changed\n").expect("modify worktree");

    let hook_dir = dir.to_path_buf();
    let hook = Arc::new(move || {
        let repo = open_grit_repo(&hook_dir);
        let req = grit_test_support::commit_request("race", "A <a@b.c> 1 +0000");
        create_commit(&repo, &req, &mut NullProgress, &NeverCancel).expect("commit in hook");
    });

    let repo = open_grit_repo(dir);
    let mut opts = StatusOptions::default();
    opts.untracked = UntrackedMode::No;
    opts.ahead_behind = false;
    opts.before_opportunistic_index_write = Some(BeforeOpportunisticIndexWriteHook(hook));
    status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("status");

    assert!(
        git_diff_cached_names(dir).is_empty(),
        "index must match HEAD after status with concurrent commit"
    );
    assert_git_fsck_strict(dir);
}

#[test]
fn try_write_index_at_skips_when_disk_index_identity_changed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);

    let repo = open_grit_repo(dir);
    let handle = repo.index_handle();
    let index_path = handle.path().to_path_buf();
    let mut index = grit_lib::index::Index::load(&index_path).expect("load index");
    fs::write(dir.join("file.txt"), "v2\n").expect("modify");
    git(dir, &["add", "file.txt"]);
    git(dir, &["commit", "-qm", "update"]);
    let on_disk_after_commit = fs::read(&index_path).expect("index after commit");

    let written = handle.try_write(&mut index).expect("try_write_index");
    assert!(!written, "stale in-memory index must not be written");
    assert_eq!(
        fs::read(&index_path).expect("index reread"),
        on_disk_after_commit,
        "on-disk index bytes must be unchanged"
    );
    assert_git_fsck_strict(dir);
}
