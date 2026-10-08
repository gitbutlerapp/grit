//! Smoke test: fetch from a bare remote (regression for grit-examples fetch_push).

use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{fetch_local, FetchOptions};

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
    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        ..Default::default()
    };
    fetch_local(&local_git, &bare, &opts).expect("fetch from bare remote");
    resolve_ref(&local_git, "refs/remotes/origin/main").expect("tracking ref");
}
