//! A commit must fail, not revert, when another process moves the branch after the commit read
//! HEAD (issue #1791). The `pre-commit` hook stands in for that process.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::refs::store::RefStoreError;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;

fn commit_req(message: &str) -> CommitRequest {
    let id = "Moved Branch <moved@example.com> 1700000000 +0000".to_owned();
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Moved Branch")
        .env("GIT_AUTHOR_EMAIL", "moved@example.com")
        .env("GIT_COMMITTER_NAME", "Moved Branch")
        .env("GIT_COMMITTER_EMAIL", "moved@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

/// Repository with `a.txt` modified in the worktree and a `pre-commit` hook that moves `main` to
/// a commit landed "elsewhere" (changing `b.txt`). Returns the repository and that commit.
fn repo_with_racing_hook(root: &Path) -> (Repository, String) {
    use std::os::unix::fs::PermissionsExt;

    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), b"1\n").expect("a");
    fs::write(root.join("b.txt"), b"1\n").expect("b");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);

    fs::write(root.join("b.txt"), b"from elsewhere\n").expect("b elsewhere");
    git(root, &["commit", "-qam", "elsewhere"]);
    let other = git(root, &["rev-parse", "HEAD"]);
    git(root, &["reset", "-q", "--hard", "HEAD~1"]);

    let hooks = root.join(".git/hooks");
    fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-commit");
    fs::write(
        &hook,
        format!("#!/bin/sh\nexec git update-ref refs/heads/main {other}\n"),
    )
    .expect("hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");

    fs::write(root.join("a.txt"), b"2\n").expect("edit a");
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    (repo, other)
}

fn assert_branch_moved_error(err: &Error) {
    assert!(
        matches!(
            err,
            Error::RefStore(RefStoreError::ExpectedMismatch { name, .. }) if name == "refs/heads/main"
        ),
        "{err:?}"
    );
}

#[test]
fn paths_commit_fails_when_branch_moves_after_head_was_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (repo, other) = repo_with_racing_hook(root);

    let err = commit_with_scope(
        &repo,
        &commit_req("mine"),
        CommitScope::Paths(ResolvedPathspecs {
            specs: vec!["a.txt".into()],
            sources: vec!["a.txt".into()],
            scoped: true,
        }),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("moved branch must fail the commit");

    assert_branch_moved_error(&err);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), other);
    assert_eq!(git(root, &["show", "HEAD:b.txt"]), "from elsewhere");
    assert!(!root.join(".git/index.lock").exists());
    git(root, &["fsck", "--strict", "--no-progress"]);
}

#[test]
fn auto_stage_commit_fails_when_branch_moves_after_head_was_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (repo, other) = repo_with_racing_hook(root);

    let err = commit_with_scope(
        &repo,
        &commit_req("mine"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("moved branch must fail the commit");

    assert_branch_moved_error(&err);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), other);
    assert_eq!(git(root, &["show", "HEAD:b.txt"]), "from elsewhere");
    git(root, &["fsck", "--strict", "--no-progress"]);
}
