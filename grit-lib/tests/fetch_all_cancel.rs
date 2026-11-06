//! `Remotes::fetch_all` aborts when the cancel token fires.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::progress::CancelToken;
use grit_lib::progress::{NullProgress, Token};
use grit_lib::remote::transfer::FetchOptions;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::path::Path;

fn seed_bare(base: &Path, name: &str) -> std::path::PathBuf {
    let bare = base.join(format!("{name}.git"));
    git(
        base,
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let wt = base.join(format!("{name}-wt"));
    git(
        base,
        &["clone", "-q", bare.to_str().unwrap(), wt.to_str().unwrap()],
    );
    git(&wt, &["commit", "-q", "--allow-empty", "-m", "c"]);
    git(&wt, &["push", "-q", "origin", "main"]);
    bare
}

#[test]
fn fetch_all_aborts_when_cancelled_on_only_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = seed_bare(tmp.path(), "origin");
    let consumer = tmp.path().join("consumer");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", consumer.to_str().unwrap()],
    );
    git(
        &consumer,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );

    let repo = Repository::discover(Some(&consumer), &RepositoryOptions::empty()).expect("open");
    let config = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(repo.git_dir()),
        &crate::config::LoadConfigOptions::standard(
            &grit_lib::config::environment::Environment::empty(),
            Some(repo.git_dir()),
        ),
    )
    .expect("config");
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut progress = NullProgress;
    let err = repo
        .remotes()
        .fetch_all(
            &config,
            &FetchOptions::default(),
            None,
            None,
            &mut progress,
            &cancel,
            None,
        )
        .expect_err("cancel should abort fetch_all");
    assert!(matches!(err, Error::Cancelled));
}

#[test]
fn fetch_all_aborts_when_only_remote_returns_cancelled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = seed_bare(tmp.path(), "origin");
    let consumer = tmp.path().join("consumer");
    git(
        tmp.path(),
        &["init", "-q", "-b", "main", consumer.to_str().unwrap()],
    );
    git(
        &consumer,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );

    let repo = Repository::discover(Some(&consumer), &RepositoryOptions::empty()).expect("open");
    let config = ConfigSet::load(
        &grit_lib::config::environment::Environment::empty(),
        Some(repo.git_dir()),
        &crate::config::LoadConfigOptions::standard(
            &grit_lib::config::environment::Environment::empty(),
            Some(repo.git_dir()),
        ),
    )
    .expect("config");
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut progress = NullProgress;
    let err = repo
        .remotes()
        .fetch_all(
            &config,
            &FetchOptions::default(),
            None,
            None,
            &mut progress,
            &cancel,
            None,
        )
        .expect_err("cancel should abort fetch_all");
    assert!(matches!(err, Error::Cancelled));
}
