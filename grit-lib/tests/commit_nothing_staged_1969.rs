//! Index-only commit with a dirty work tree must not report a clean tree (issue #1969).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::commit::{
    commit_with_scope, default_commit_scope, CommitRequest, CommitScope,
};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;

fn commit_req(message: &str) -> CommitRequest {
    let id = "Test <t@example.com> 1700000000 +0000";
    {
        let mut o = CommitRequest::default();
        o.message = message.to_owned();
        o.author = Some(id.parse().expect("signature"));
        o.committer = Some(id.parse().expect("signature"));
        o
    }
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
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

fn repo_with_autostage_off() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git_out(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a"), "a\n").expect("write a");
    git_out(&root, &["add", "a"]);
    git_out(&root, &["commit", "-q", "-m", "base"]);
    git_out(&root, &["config", "commit.autoStage", "false"]);
    std::fs::write(root.join("a"), "a\nmore\n").expect("modify a");
    std::fs::write(root.join("n"), "new\n").expect("write n");
    (tmp, root)
}

#[test]
fn staged_scope_reports_nothing_staged_when_worktree_dirty() {
    let (_tmp, root) = repo_with_autostage_off();
    let repo = open_grit(&root);
    let err = commit_with_scope(
        &repo,
        &commit_req("x"),
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("commit");
    match err {
        Error::NothingStagedToCommit {
            unstaged,
            untracked,
        } => {
            assert_eq!(unstaged, 1, "modified a");
            assert_eq!(untracked, 1, "untracked n");
        }
        other => panic!("expected NothingStagedToCommit, got {other:?}"),
    }
}

#[test]
fn default_scope_with_autostage_off_is_staged_and_reports_nothing_staged() {
    let (_tmp, root) = repo_with_autostage_off();
    let repo = open_grit(&root);
    assert_eq!(
        default_commit_scope(&repo).expect("scope"),
        CommitScope::Staged
    );
    let err = commit_with_scope(
        &repo,
        &commit_req("x"),
        default_commit_scope(&repo).expect("scope"),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("commit");
    assert!(
        matches!(err, Error::NothingStagedToCommit { .. }),
        "got {err:?}"
    );
}

#[test]
fn paths_scope_with_other_paths_staged_reports_nothing_to_commit_not_false_unstaged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "a\n").expect("write a");
    std::fs::write(root.join("b.txt"), "b\n").expect("write b");
    git_out(root, &["add", "."]);
    git_out(root, &["commit", "-q", "-m", "base"]);
    std::fs::write(root.join("b.txt"), "b\nmore\n").expect("modify b");
    git_out(root, &["add", "b.txt"]);

    let repo = open_grit(root);
    let paths = ResolvedPathspecs {
        specs: vec!["a.txt".into()],
        sources: vec!["a.txt".into()],
        scoped: true,
    };
    let err = commit_with_scope(
        &repo,
        &commit_req("nope"),
        CommitScope::Paths(paths),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("path a.txt has nothing to commit");
    assert!(
        matches!(err, Error::NothingToCommit),
        "must not treat staged b.txt as unstaged in the path index: {err:?}"
    );
}

#[test]
fn clean_tree_still_reports_nothing_to_commit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a"), "a\n").expect("write");
    git_out(root, &["add", "a"]);
    git_out(root, &["commit", "-q", "-m", "base"]);
    git_out(root, &["config", "commit.autoStage", "false"]);
    let repo = open_grit(root);
    let err = commit_with_scope(
        &repo,
        &commit_req("x"),
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("commit");
    assert!(matches!(err, Error::NothingToCommit), "got {err:?}");
}

fn init_nested_repo(path: &Path) {
    git_out(path, &["init", "-q", "-b", "main"]);
    git_out(path, &["config", "user.email", "t@example.com"]);
    git_out(path, &["config", "user.name", "Test"]);
}

#[test]
fn staged_scope_counts_submodule_untracked_as_unstaged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    git_out(root, &["config", "user.email", "t@example.com"]);
    git_out(root, &["config", "user.name", "Test"]);

    let inner = root.join("inner");
    std::fs::create_dir_all(&inner).expect("mkdir inner");
    init_nested_repo(&inner);
    std::fs::write(inner.join("tracked.txt"), "v1\n").expect("write");
    git_out(&inner, &["add", "tracked.txt"]);
    git_out(&inner, &["commit", "-q", "-m", "inner"]);
    let inner_head = git_out(&inner, &["rev-parse", "HEAD"]).trim().to_owned();
    git_out(&inner, &["checkout", "-q", &inner_head]);

    git_out(root, &["add", "inner"]);
    git_out(root, &["commit", "-q", "-m", "add submodule"]);
    git_out(root, &["config", "commit.autoStage", "false"]);

    std::fs::write(inner.join("untracked.txt"), "new\n").expect("untracked in submodule");

    let repo = open_grit(root);
    let err = commit_with_scope(
        &repo,
        &commit_req("x"),
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("commit");
    match err {
        Error::NothingStagedToCommit { unstaged, .. } => {
            assert!(
                unstaged >= 1,
                "submodule untracked content must count as unstaged, got {err:?}"
            );
        }
        other => panic!("expected NothingStagedToCommit, got {other:?}"),
    }
}
