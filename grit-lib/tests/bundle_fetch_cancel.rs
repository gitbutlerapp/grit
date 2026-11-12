//! Cancellation during bundle fetch.

use grit_lib::error::Error;
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_lib::repo::Repository;
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

    let flag = CancelToken::new();
    flag.cancel();

    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::Following;
    let err = Repository::open(
        &dest.join(".git"),
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .bundles()
    .fetch(&bundle, &opts, &mut NullProgress, &flag)
    .expect_err("cancelled before bundle ingest");
    assert!(matches!(err, Error::Cancelled));
}
