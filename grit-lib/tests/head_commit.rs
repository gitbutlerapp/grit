//! [`Repository::head_commit`] on repositories written by the system `git`, on an injected ref
//! store, and through the history engine that relies on it.

#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectId};
use grit_lib::refs::store::{
    Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::{Repository, RepositoryBuilder};
use grit_lib::revwalk::history::{
    move_commit, EmptyPolicy, HistoryError, Placement, RefScope, RewriteOptions,
};
use grit_test_support::git;

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("discover")
}

fn git_head(dir: &Path) -> ObjectId {
    git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("HEAD oid")
}

fn repo_with_two_commits() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    for name in ["a", "b"] {
        fs::write(dir.path().join(name), name).expect("write");
        git(dir.path(), &["add", name]);
        git(dir.path(), &["commit", "-q", "-m", name]);
    }
    dir
}

#[test]
fn branch_head_matches_git() {
    let dir = repo_with_two_commits();
    assert_eq!(
        open(dir.path()).head_commit().expect("head"),
        git_head(dir.path())
    );
}

#[test]
fn detached_head_matches_git() {
    let dir = repo_with_two_commits();
    git(dir.path(), &["checkout", "-q", "--detach", "HEAD~1"]);
    assert_eq!(
        open(dir.path()).head_commit().expect("head"),
        git_head(dir.path())
    );
}

#[test]
fn unborn_branch_is_unborn_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let err = open(dir.path()).head_commit().expect_err("unborn");
    assert!(matches!(err, Error::UnbornHead), "{err:?}");
}

#[test]
fn head_naming_a_missing_non_branch_ref_is_invalid_head() {
    let dir = repo_with_two_commits();
    let repo = open(dir.path());
    fs::write(repo.git_dir().join("HEAD"), "ref: refs/tags/missing\n").expect("write HEAD");
    let err = repo.head_commit().expect_err("invalid");
    assert!(matches!(err, Error::InvalidHead), "{err:?}");
}

#[test]
fn injected_ref_store_is_the_source_of_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let refs = Arc::new(MemoryRefStore::new());
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(dir.path().join("objects"))
                .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                .alternates(false),
        )
        .ref_store(refs.clone())
        .initial_branch("main")
        .open(dir.path(), None)
        .expect("open injected");
    let err = repo.head_commit().expect_err("unborn");
    assert!(matches!(err, Error::UnbornHead), "{err:?}");

    let oid: ObjectId = "0100000000000000000000000000000000000000"
        .parse()
        .expect("oid");
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(oid)),
            expected: Expected::Any,
            reflog: None,
            flags: RefUpdateFlags::default(),
        })
        .expect("txn");
    refs.prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
    assert_eq!(repo.head_commit().expect("detached"), oid);
}

#[test]
fn history_planning_reports_unborn_head_as_typed_library_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let repo = open(dir.path());
    let commit = ObjectId::null(HashAlgo::Sha1);
    let opts = RewriteOptions {
        scope: RefScope::Head,
        empty: EmptyPolicy::Drop,
        committer: "T <t@grit.test> 1 +0000".parse().expect("signature"),
        preserve_committer: false,
        sign: false,
    };
    let err = move_commit(&repo, commit, Placement::Before(commit), &opts).expect_err("unborn");
    assert!(
        matches!(&err, HistoryError::Library(inner) if matches!(**inner, Error::UnbornHead)),
        "{err:?}"
    );
}

#[test]
fn history_error_from_library_error_unwraps_history_failures() {
    assert!(matches!(
        HistoryError::from(Error::History(HistoryError::NotLinear)),
        HistoryError::NotLinear
    ));
    assert!(matches!(
        HistoryError::from(Error::UnbornHead),
        HistoryError::Library(inner) if matches!(*inner, Error::UnbornHead)
    ));
}
