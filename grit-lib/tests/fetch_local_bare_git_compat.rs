//! `fetch_local` from a `git clone --bare` remote (guide_network layout).

use grit_lib::remote::transfer::{FetchOptions, TagMode};
use grit_test_support::git;

#[test]
fn fetch_local_from_bare_remote_like_guide_network() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).expect("work dir");
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"]);

    let bare = tmp.path().join("repo.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );

    let consumer = tmp.path().join("consumer");
    std::fs::create_dir_all(&consumer).expect("consumer dir");
    git(&consumer, &["init", "-q", "-b", "main", "."]);
    let local_git = consumer.join(".git");

    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
    opts.tags = TagMode::Following;
    grit_lib::repo::Repository::open(
        &local_git,
        &grit_lib::config::environment::RepositoryOptions::with_environment(
            (&opts).environment.clone(),
        ),
    )
    .expect("open fetch target")
    .remotes()
    .fetch_from(
        grit_lib::remote::transfer::TransferEndpoint::Local(&bare),
        &mut opts.clone(),
        &mut grit_lib::remote::fetch::NoProgress,
        grit_lib::progress::CancelToken::never_ref(),
    )
    .expect("fetch_local from bare remote");
    grit_test_support::refs::at(&local_git)
        .references()
        .resolve("refs/remotes/origin/main")
        .expect("tracking ref");
}
