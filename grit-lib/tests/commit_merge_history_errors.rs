//! Typed commit and merge area errors (error contract step 9).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::error::RefError;
use grit_lib::merge::{ContinueOptions, FastForwardPolicy, IntegrateBranchOutcome, MergeOptions};
use grit_lib::merge::{ReplayEmptyPolicy, ReplayOptions, ReplaySequenceRequest};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::commit::{CommitRequest, CommitScope};
use grit_lib::porcelain::replay::ReplayDirection;
use grit_lib::porcelain::{CommitError, MergeError};
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::state::InProgressOperation;
use grit_lib::worktree::pathspec::{Pathspec, PathspecGlobalFlags};
use grit_test_support::start_replay_sequence;
use grit_test_support::{continue_merge, integrate_branch, integrate_branch_with};
use std::path::Path;
use std::process::Command;

const IDENT: &str = "T <t@example.com> 1700000000 +0000";

fn test_ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("IDENT")
}

fn continue_options() -> ContinueOptions {
    let id = test_ident();
    ContinueOptions::default()
        .with_author(Some(id.clone()))
        .with_committer(Some(id))
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

    let err = repo
        .commits()
        .create(
            &grit_lib::commits::CommitOptions::new(
                CommitRequest::new("   \n".to_owned()),
                CommitScope::Staged,
            ),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::Commit(CommitError::EmptyCommitMessage)
    ));

    let err = repo
        .commits()
        .create(
            &grit_lib::commits::CommitOptions::new(
                CommitRequest::new("x".to_owned()),
                CommitScope::Paths(Pathspec::entire_tree(PathspecGlobalFlags::default())),
            ),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::Commit(CommitError::CommitPathsRequired)
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
        &MergeOptions::default().with_fast_forward(FastForwardPolicy::Only),
        &mut NullProgress,
        &CancelToken::never(),
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
        &CancelToken::never(),
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

    let err = continue_merge(
        &repo,
        &continue_options(),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        grit_lib::error::Error::Merge(MergeError::ContinueUnmergedPaths { .. })
            | grit_lib::error::Error::Index(_)
    ));

    let side_tip = topic;
    let pick_err = start_replay_sequence(
        &repo,
        &ReplaySequenceRequest::new(vec![side_tip], ReplayDirection::Pick, test_ident())
            .with_options(ReplayOptions::default()),
        &mut NullProgress,
        &CancelToken::never(),
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
    assert!(
        !src.contains("\n    Library("),
        "HistoryError must not wrap the umbrella Error as Library(...)"
    );
    assert!(
        !src.contains("Planning { message"),
        "HistoryError must not use stringly Planning {{ message: ... }}"
    );
}

#[test]
fn history_failures_from_other_areas_keep_their_area() {
    let err = Error::Ref(RefError::InvalidHead);
    assert_eq!(err.area(), "ref");
    let history = Error::from(grit_lib::revwalk::history::HistoryError::NothingToReword);
    assert_eq!(history.area(), "history");
    assert!(matches!(
        history,
        Error::History(grit_lib::revwalk::history::HistoryError::NothingToReword)
    ));
}
