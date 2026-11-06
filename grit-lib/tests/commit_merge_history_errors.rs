//! Typed commit and merge area errors (error contract step 9).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::merge::{
    continue_merge, integrate_branch, integrate_branch_with, ContinueOptions, FastForwardPolicy,
    IntegrateBranchOutcome, MergeOptions,
};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::replay_sequence::{
    start_replay_sequence, ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest,
};
use grit_lib::porcelain::{CommitError, MergeError};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::state::InProgressOperation;
use grit_lib::worktree::pathspec::ResolvedPathspecs;
use std::path::Path;
use std::process::Command;

const IDENT: &str = "T <t@example.com> 1700000000 +0000";

fn test_ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("IDENT")
}

fn continue_options() -> ContinueOptions {
    let id = test_ident();
    ContinueOptions {
        message: None,
        author: Some(id.clone()),
        committer: Some(id),
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .current_dir(dir)
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

fn diverged_repo() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "1\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "main"]);
    git(root, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(root.join("f"), "2\n").unwrap();
    git(root, &["commit", "-am", "topic"]);
    let topic = ObjectId::from_hex(git(root, &["rev-parse", "HEAD"]).trim()).unwrap();
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f"), "3\n").unwrap();
    git(root, &["commit", "-am", "main2"]);
    let main_tip = ObjectId::from_hex(git(root, &["rev-parse", "main"]).trim()).unwrap();
    (tmp, main_tip, topic)
}

#[test]
fn commit_error_variants() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("f"), "1\n").unwrap();
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "seed"]);
    std::fs::write(root.join("f"), "2\n").unwrap();
    git(root, &["add", "f"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();

    let err = commit_with_scope(
        &repo,
        &CommitRequest::new("   \n".to_owned()),
        CommitScope::Staged,
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Commit(CommitError::EmptyCommitMessage)
    ));

    let err = commit_with_scope(
        &repo,
        &CommitRequest::new("x".to_owned()),
        CommitScope::Paths(ResolvedPathspecs::default()),
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Commit(CommitError::CommitPathsRequired)
    ));
}

#[test]
fn merge_error_variants() {
    let (tmp, main, topic) = diverged_repo();
    let root = tmp.path();
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();

    let err = integrate_branch_with(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &MergeOptions {
            fast_forward: FastForwardPolicy::Only,
            ..MergeOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(err, Error::Merge(MergeError::NotFastForward)));

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        topic,
        "topic",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    let err =
        continue_merge(&repo, &continue_options(), &mut NullProgress, &NeverCancel).unwrap_err();
    assert!(matches!(
        err,
        MergeError::ContinueUnmergedPaths { .. } | MergeError::Library(_)
    ));

    let side_tip = topic;
    let pick_err = start_replay_sequence(
        &repo,
        &ReplaySequenceRequest {
            direction: ReplayDirection::Pick,
            commits: vec![side_tip],
            committer: test_ident(),
            options: ReplayOptions {
                empty: ReplayEmptyPolicy::Stop,
                ..ReplayOptions::default()
            },
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .unwrap_err();
    assert!(matches!(
        pick_err,
        Error::Merge(MergeError::OperationInProgress(InProgressOperation::Merge))
    ));
}

#[test]
fn history_error_has_no_string_catch_all() {
    let src = include_str!("../src/history/error.rs");
    assert!(
        !src.contains("Message(String)"),
        "HistoryError must not define Message(String)"
    );
}
