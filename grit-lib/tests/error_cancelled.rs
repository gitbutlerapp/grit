//! Cancellation is [`Error::Cancelled`] at every layer; use [`Error::is_cancelled`] and the
//! matching helper on operation error types instead of walking nested wrappers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::bundle::BundleError;
use grit_lib::error::Error;
use grit_lib::porcelain::pull::PullError;
use grit_lib::progress::FlagCancel;
use grit_lib::remote::clone::{clone, CloneError, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{fetch_local_with_progress, FetchOptions, TagMode};
use grit_lib::remote::RemoteError;
use grit_lib::revwalk::history::{HistoryError, RebaseSessionError};
use grit_test_support::git;

fn cancelled() -> Error {
    Error::Cancelled
}

#[test]
fn wrapper_helpers_classify_only_cancelled() {
    assert!(cancelled().is_cancelled());
    assert!(!Error::nothing_to_commit_clean().is_cancelled());

    assert!(RemoteError::Library(cancelled()).is_cancelled());
    assert!(!RemoteError::NoUrl { remote: None }.is_cancelled());

    assert!(CloneError::Library(cancelled()).is_cancelled());
    assert!(CloneError::Remote(RemoteError::Library(cancelled())).is_cancelled());
    assert!(!CloneError::InvalidCloneDepth.is_cancelled());
    assert!(!CloneError::RemoteRefNotFound("x".into()).is_cancelled());

    assert!(PullError::Remote(RemoteError::Library(cancelled())).is_cancelled());
    assert!(PullError::Repository(cancelled()).is_cancelled());
    assert!(!PullError::DetachedHead.is_cancelled());

    assert!(HistoryError::Library(Box::new(cancelled())).is_cancelled());
    assert!(!HistoryError::EmptyMessage.is_cancelled());

    assert!(RebaseSessionError::Library(cancelled()).is_cancelled());
    assert!(
        RebaseSessionError::History(HistoryError::Library(Box::new(cancelled()))).is_cancelled()
    );
    assert!(!RebaseSessionError::NoRebaseInProgress.is_cancelled());

    assert!(BundleError::Cancelled.is_cancelled());
    assert!(!BundleError::BadSignature.is_cancelled());

    assert!(Error::History(HistoryError::Library(Box::new(cancelled()))).is_cancelled());
}

#[test]
fn fetch_local_cancelled_at_start_is_classified() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("bare.git");
    let local = tmp.path().join("local");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare.to_str().unwrap()],
    );
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", local.to_str().unwrap()],
    );
    git(&local, &["commit", "-q", "--allow-empty", "-m", "c1"]);

    let flag = FlagCancel::new();
    flag.cancel();

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    let err = fetch_local_with_progress(&local.join(".git"), &bare, &opts, &mut NoProgress, &flag)
        .expect_err("cancelled before fetch");
    assert!(err.is_cancelled(), "{err:?}");
}

#[test]
fn clone_cancelled_at_start_is_classified_and_needs_cleanup() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    let dest = tmp.path().join("dest");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", upstream.to_str().unwrap()],
    );
    git(&upstream, &["commit", "-q", "--allow-empty", "-m", "c1"]);

    let flag = FlagCancel::new();
    flag.cancel();

    let opts = CloneOptions::new(upstream.to_string_lossy(), dest.clone());
    let err = clone(opts, &mut NoProgress, &flag, None).expect_err("cancelled before clone");
    assert!(matches!(
        err,
        CloneError::Remote(RemoteError::Library(Error::Cancelled))
    ));
    assert!(err.is_cancelled(), "{err:?}");
    assert!(err.should_cleanup_failed_clone(), "{err:?}");
    assert!(dest.join(".git").is_dir(), "partial clone leaves .git");
}
