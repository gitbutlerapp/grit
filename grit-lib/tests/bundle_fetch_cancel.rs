//! Cancellation during bundle fetch.

use grit_lib::bundle::bundle_remote::fetch_from_bundle_with_progress;
use grit_lib::error::Error;
use grit_lib::progress::FlagCancel;
use grit_lib::remote::fetch::NoProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_test_support::git;

#[test]
fn bundle_fetch_returns_cancelled_when_signalled_before_unbundle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", upstream.to_str().unwrap()],
    );
    git(&upstream, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    let bundle = tmp.path().join("repo.bundle");
    git(
        &upstream,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/main",
        ],
    );

    let dest = tmp.path().join("dest");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", dest.to_str().unwrap()],
    );
    git(&dest, &["commit", "-q", "--allow-empty", "-m", "seed"]);

    let flag = FlagCancel::new();
    flag.cancel();

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    let err =
        fetch_from_bundle_with_progress(&dest.join(".git"), &bundle, &opts, &mut NoProgress, &flag)
            .expect_err("cancelled before bundle ingest");
    assert!(matches!(err, Error::Cancelled));
}
