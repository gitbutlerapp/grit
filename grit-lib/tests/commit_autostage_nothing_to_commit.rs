//! Auto-staging commits that end up with nothing to record must still persist the staged
//! index, so system `git` sees index == HEAD afterwards (issue #1842).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

fn commit_req(message: &str) -> CommitRequest {
    let id = "AutoStage <auto@example.com> 1700000000 +0000";
    CommitRequest {
        message: message.to_owned(),
        author: Some(id.parse().expect("signature")),
        committer: Some(id.parse().expect("signature")),
        ..Default::default()
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "AutoStage")
        .env("GIT_AUTHOR_EMAIL", "auto@example.com")
        .env("GIT_COMMITTER_NAME", "AutoStage")
        .env("GIT_COMMITTER_EMAIL", "auto@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

/// Repository with one commit containing `x.txt`.
fn repo_with_initial_commit() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("x.txt"), "x\n").expect("write");
    git_out(root, &["add", "x.txt"]);
    git_out(root, &["commit", "-q", "-m", "x"]);
    tmp
}

fn assert_nothing_to_commit(root: &Path, req: &CommitRequest) {
    let head_before = git_out(root, &["rev-parse", "HEAD"]);
    let err = commit_with_scope(
        &open_grit(root),
        req,
        CommitScope::AutoStage,
        &mut NullProgress,
    )
    .expect_err("nothing to commit");
    assert!(matches!(err, Error::NothingToCommit { .. }), "got {err:?}");
    assert_eq!(git_out(root, &["rev-parse", "HEAD"]), head_before);
}

#[test]
fn staged_new_file_deleted_from_worktree_is_dropped_from_index() {
    let tmp = repo_with_initial_commit();
    let root = tmp.path();
    std::fs::write(root.join("n.txt"), "stg\n").expect("write");
    git_out(root, &["add", "n.txt"]);
    std::fs::remove_file(root.join("n.txt")).expect("rm");
    assert_eq!(git_out(root, &["status", "--porcelain"]), "AD n.txt\n");

    assert_nothing_to_commit(root, &commit_req("c"));

    assert_eq!(git_out(root, &["status", "--porcelain"]), "");
    assert_eq!(git_out(root, &["ls-files"]), "x.txt\n");
    git_out(root, &["diff", "--cached", "--quiet", "HEAD"]);
    git_out(root, &["fsck", "--strict"]);
}

#[test]
fn staged_modification_reverted_in_worktree_restores_head_entry() {
    let tmp = repo_with_initial_commit();
    let root = tmp.path();
    std::fs::write(root.join("x.txt"), "changed\n").expect("write");
    git_out(root, &["add", "x.txt"]);
    std::fs::write(root.join("x.txt"), "x\n").expect("write");
    assert_eq!(git_out(root, &["status", "--porcelain"]), "MM x.txt\n");

    assert_nothing_to_commit(root, &commit_req("c"));

    assert_eq!(git_out(root, &["status", "--porcelain"]), "");
    assert_eq!(
        git_out(root, &["ls-files", "-s", "x.txt"]),
        git_out(root, &["ls-tree", "HEAD", "--", "x.txt"])
            .replace("100644 blob ", "100644 ")
            .replace('\t', " 0\t")
    );
    git_out(root, &["fsck", "--strict"]);
}

#[test]
fn dry_run_leaves_index_untouched() {
    let tmp = repo_with_initial_commit();
    let root = tmp.path();
    std::fs::write(root.join("n.txt"), "stg\n").expect("write");
    git_out(root, &["add", "n.txt"]);
    std::fs::remove_file(root.join("n.txt")).expect("rm");
    let index_before = std::fs::read(root.join(".git/index")).expect("index");

    let mut req = commit_req("c");
    req.dry_run = true;
    assert_nothing_to_commit(root, &req);

    assert_eq!(
        std::fs::read(root.join(".git/index")).expect("index"),
        index_before
    );
    assert_eq!(git_out(root, &["status", "--porcelain"]), "AD n.txt\n");
}
