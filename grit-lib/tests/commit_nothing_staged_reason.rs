//! Commit failures distinguish a clean tree from unstaged-only work (issue #1976).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, NothingToCommitReason};
use grit_lib::porcelain::commit::{
    commit_with_scope, default_commit_scope_from_config, CommitRequest, CommitScope,
};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

fn commit_req(message: &str) -> CommitRequest {
    let id = "T <t@e.x> 1700000000 +0000";
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
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
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

#[test]
fn staged_scope_with_only_unstaged_edits_reports_uncommitted_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "1\n").expect("write");
    git_out(root, &["add", "a.txt"]);
    git_out(root, &["commit", "-q", "-m", "one"]);
    std::fs::write(root.join("a.txt"), "1\n2\n").expect("append");

    let err = commit_with_scope(
        &open_grit(root),
        &commit_req("nope"),
        CommitScope::Staged,
        &mut NullProgress,
    )
    .expect_err("nothing staged");
    match err {
        Error::NothingToCommit {
            reason:
                NothingToCommitReason::Uncommitted {
                    unstaged,
                    untracked,
                    sample_paths,
                },
        } => {
            assert_eq!(unstaged, 1);
            assert_eq!(untracked, 0);
            assert_eq!(sample_paths, ["a.txt"]);
        }
        other => panic!("expected Uncommitted reason, got {other:?}"),
    }
}

#[test]
fn auto_stage_disabled_uses_staged_scope_and_same_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git_out(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "1\n").expect("write");
    git_out(root, &["add", "a.txt"]);
    git_out(root, &["commit", "-q", "-m", "one"]);
    std::fs::write(root.join("a.txt"), "1\n2\n").expect("append");

    let repo = open_grit(root);
    let config = repo.config().expect("config");
    assert!(matches!(
        default_commit_scope_from_config(config.as_ref()),
        CommitScope::AutoStage
    ));
    let mut cfg = config.as_ref().clone();
    cfg.add_command_override("commit.autoStage", "false")
        .expect("override");
    assert!(matches!(
        default_commit_scope_from_config(&cfg),
        CommitScope::Staged
    ));

    let err = commit_with_scope(
        &repo,
        &commit_req("nope"),
        CommitScope::Staged,
        &mut NullProgress,
    )
    .expect_err("autoStage off");
    assert!(matches!(
        err,
        Error::NothingToCommit {
            reason: NothingToCommitReason::Uncommitted { .. },
        }
    ));
}
