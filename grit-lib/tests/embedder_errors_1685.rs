//! Typed errors for common embedder failures (issue #1685).

use grit_lib::blame::{self, compute_blame, BlameDiffAlgorithm};
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::staging::stage_worktree_changes;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_parse_error::RevParseError;
use std::collections::{HashMap, HashSet};
fn assert_requires_work_tree(result: Result<(), Error>) {
    assert!(
        matches!(result, Err(Error::RequiresWorkTree)),
        "expected RequiresWorkTree, got {result:?}"
    );
}

#[test]
fn bare_repo_common_failures_use_requires_work_tree() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = Repository::init(
        &base.path().join("bare"),
        &InitOptions::default().with_bare(true),
    )
    .expect("init bare");

    assert_requires_work_tree(
        status(
            &bare,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .map(|_| ()),
    );
    assert_requires_work_tree(stage_worktree_changes(&bare, &[]).map(|_| ()));

    let mut idx = bare.index_handle().load().expect("index");
    assert_requires_work_tree(
        bare.diff()
            .worktree_to_index(&mut idx, false, false)
            .map(|_| ()),
    );
}

fn repo_with_one_commit() -> (tempfile::TempDir, Repository, ObjectId) {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        &base.path().join("w"),
        &InitOptions::default().with_initial_branch("main"),
    )
    .expect("init");
    std::fs::write(base.path().join("w/a"), "x\n").expect("write");
    stage_worktree_changes(&repo, &[]).expect("stage");
    let req = grit_test_support::commit_request("m\n", "A <a@b> 0 +0000");
    let outcome = create_commit(&repo, &req, &mut NullProgress, &NeverCancel).expect("commit");
    (base, repo, outcome.oid)
}

fn assert_blame_path_not_in_revision(err: Error, expected_path: &str, expected_revision: &str) {
    assert!(
        matches!(
            err,
            Error::RevParse(RevParseError::PathNotInRevision { ref path, ref revision, .. })
                if path == expected_path && revision == expected_revision
        ),
        "got {err:?}"
    );
}

#[test]
fn blame_file_missing_path_is_path_not_in_revision() {
    let (_base, repo, _head) = repo_with_one_commit();
    let err = blame::blame_file(&repo, "nope.txt", &Default::default()).unwrap_err();
    assert_blame_path_not_in_revision(err, "nope.txt", "HEAD");
}

#[test]
fn compute_blame_missing_path_is_path_not_in_revision() {
    let (_base, repo, head) = repo_with_one_commit();
    let err = compute_blame(
        repo.odb(),
        head,
        "missing.txt",
        &HashSet::new(),
        BlameDiffAlgorithm::Myers,
        None,
        false,
        0,
        false,
        false,
        &HashMap::new(),
        false,
        None,
        grit_lib::diff::WhitespaceMode::default(),
        &ThreadPolicy::default(),
    )
    .unwrap_err();
    assert_blame_path_not_in_revision(err, "missing.txt", &head.to_hex());
}

#[test]
fn deep_parent_navigation_is_revision_out_of_range() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        &base.path().join("w"),
        &InitOptions::default().with_initial_branch("main"),
    )
    .expect("init");
    std::fs::write(base.path().join("w/a"), "x\n").expect("write");
    stage_worktree_changes(&repo, &[]).expect("stage");
    let req = grit_test_support::commit_request("m\n", "A <a@b> 0 +0000");
    create_commit(&repo, &req, &mut NullProgress, &NeverCancel).expect("commit");

    let err = repo.rev_walk().resolve("main~99").unwrap_err();
    assert!(
        matches!(
            err,
            Error::RevParse(RevParseError::RevisionOutOfRange { ref spec, .. }) if spec == "main~99"
        ),
        "got {err:?}"
    );
}

#[test]
fn empty_commit_message_is_typed() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        &base.path().join("w"),
        &InitOptions::default().with_initial_branch("main"),
    )
    .expect("init");
    std::fs::write(base.path().join("w/a"), "x\n").expect("write");
    stage_worktree_changes(&repo, &[]).expect("stage");
    let req = grit_test_support::commit_request("m\n", "A <a@b> 0 +0000");
    create_commit(&repo, &req, &mut NullProgress, &NeverCancel).expect("commit");

    let mut empty = CommitRequest::new("");
    empty.allow_empty = true;
    let err = create_commit(&repo, &empty, &mut NullProgress, &NeverCancel).unwrap_err();
    assert!(matches!(err, Error::EmptyCommitMessage), "got {err:?}");
}
