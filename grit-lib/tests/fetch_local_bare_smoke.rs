//! Smoke test: fetch from a bare remote (regression for grit-examples fetch_push).

use grit_lib::remote::transfer::FetchOptions;

#[test]
fn fetch_local_from_bare_remote_with_loose_objects_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    grit_test_support::git(&work, &["init", "-q", "-b", "main", "."]);
    grit_test_support::git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    let bare = tmp.path().join("repo.git");
    grit_test_support::git(
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
    std::fs::create_dir_all(&consumer).unwrap();
    grit_test_support::git(&consumer, &["init", "-q", "-b", "main", "."]);
    let local_git = consumer.join(".git");
    let mut opts = FetchOptions::new();
    opts.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
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
    .expect("fetch from bare remote");
    grit_test_support::refs::at(&local_git)
        .references()
        .resolve("refs/remotes/origin/main")
        .expect("tracking ref");
}
