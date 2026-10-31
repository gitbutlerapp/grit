//! Cancellation during local fetch.

use grit_lib::error::Error;
use grit_lib::progress::FlagCancel;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{fetch_local_with_progress, FetchOptions, TagMode};
use grit_test_support::git;

#[test]
fn fetch_local_returns_cancelled_when_signalled_before_start() {
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

    let opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::Following;
        o
    };
    let err = fetch_local_with_progress(&local.join(".git"), &bare, &opts, &mut NoProgress, &flag)
        .expect_err("cancelled before fetch");
    assert!(matches!(err, Error::Cancelled));
}
