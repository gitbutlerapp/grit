//! Typed errors for common embedder failures (issue #1685).

use grit_lib::blame::{self, compute_blame, BlameDiffAlgorithm};
use grit_lib::diff::TreeDiffOptions;
use grit_lib::error::Error;
use grit_lib::error::RevisionError;
use grit_lib::objects::ObjectId;
use grit_lib::parallel::ThreadPolicy;
use grit_lib::porcelain::commit::CommitRequest;
use grit_lib::porcelain::status::StatusOptions;
use grit_lib::porcelain::CommitError;
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository, RepositoryError};
use grit_lib::revwalk::rev_parse_error::RevParseError;
use std::collections::{HashMap, HashSet};
fn assert_requires_work_tree(result: Result<(), Error>) {
    assert!(
        matches!(
            result,
            Err(Error::Repository(RepositoryError::RequiresWorkTree))
        ),
        "expected RequiresWorkTree, got {result:?}"
    );
}

#[test]
fn bare_repo_common_failures_use_requires_work_tree() {
    let base = tempfile::tempdir().expect("tempdir");
    let (bare, _) = Repository::init(
        &base.path().join("bare"),
        &InitOptions::default().with_bare(true),
    )
    .expect("init bare");

    assert_requires_work_tree(
        bare.status()
            .run(
                &StatusOptions::default(),
                &mut NullProgress,
                &CancelToken::never(),
            )
            .map(|_| ()),
    );
    assert_requires_work_tree(bare.index().stage_worktree_paths(&[]).map(|_| ()));

    let mut idx = bare
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    assert_requires_work_tree(
        bare.diff()
            .index_to_worktree(
                &mut idx,
                &TreeDiffOptions::default(),
                &mut grit_lib::progress::NullProgress,
                &grit_lib::progress::CancelToken::never(),
            )
            .map(|r| r.entries)
            .map(|_| ()),
    );
}

fn repo_with_one_commit() -> (tempfile::TempDir, Repository, ObjectId) {
    let base = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        &base.path().join("w"),
        &InitOptions::default().with_initial_branch("main"),
    )
    .expect("init");
    std::fs::write(base.path().join("w/a"), "x\n").expect("write");
    &repo.index().stage_worktree_paths(&[]).expect("stage");
    let req = grit_test_support::commit_request("m\n", "A <a@b> 0 +0000");
    let outcome = repo
        .commits()
        .create_staged(&req, &mut NullProgress, &CancelToken::never())
        .expect("commit");
    (base, repo, outcome.oid)
}

fn assert_blame_path_not_in_revision(err: Error, expected_path: &str, expected_revision: &str) {
    assert!(
        matches!(
            err,
            Error::Revision(RevisionError::Parse(RevParseError::PathNotInRevision { ref path, ref revision, .. }))
                if path == expected_path && revision == expected_revision
        ),
        "got {err:?}"
    );
}

#[test]
fn blame_file_missing_path_is_path_not_in_revision() {
    let (_base, repo, _head) = repo_with_one_commit();
    let err = repo
        .diff()
        .blame(
            "nope.txt",
            &Default::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .unwrap_err();
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
    let (repo, _) = Repository::init(
        &base.path().join("w"),
        &InitOptions::default().with_initial_branch("main"),
    )
    .expect("init");
    std::fs::write(base.path().join("w/a"), "x\n").expect("write");
    &repo.index().stage_worktree_paths(&[]).expect("stage");
    let req = grit_test_support::commit_request("m\n", "A <a@b> 0 +0000");
    repo.commits()
        .create_staged(&req, &mut NullProgress, &CancelToken::never())
        .expect("commit");

    let err = repo
        .revwalk()
        .resolve("main~99", &grit_lib::revwalk::ResolveOptions::default())
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::Revision(RevisionError::Parse(RevParseError::RevisionOutOfRange { ref spec, .. })) if spec == "main~99"
        ),
        "got {err:?}"
    );
}

#[test]
fn empty_commit_message_is_typed() {
    let base = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(
        &base.path().join("w"),
        &InitOptions::default().with_initial_branch("main"),
    )
    .expect("init");
    std::fs::write(base.path().join("w/a"), "x\n").expect("write");
    &repo.index().stage_worktree_paths(&[]).expect("stage");
    let req = grit_test_support::commit_request("m\n", "A <a@b> 0 +0000");
    repo.commits()
        .create_staged(&req, &mut NullProgress, &CancelToken::never())
        .expect("commit");

    let mut empty = CommitRequest::new("");
    empty.allow_empty = true;
    let err = repo
        .commits()
        .create_staged(&empty, &mut NullProgress, &CancelToken::never())
        .unwrap_err();
    assert!(
        matches!(err, Error::Commit(CommitError::EmptyCommitMessage)),
        "got {err:?}"
    );
}
