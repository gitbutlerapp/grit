//! Cancellation during local fetch.

use grit_lib::error::Error;
use grit_lib::progress::CancelToken;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
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

    let flag = CancelToken::new();
    flag.cancel();

    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::Following;
    let err = grit_lib::repo::Repository::open(
        &local.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &mut opts.clone(),
        &mut NoProgress,
        &flag,
    )
    .expect_err("cancelled before fetch");
    assert!(matches!(err, Error::Cancelled));
}
