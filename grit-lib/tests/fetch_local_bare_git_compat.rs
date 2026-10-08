//! `fetch_local` from a `git clone --bare` remote (guide_network layout).

use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{fetch_local, FetchOptions, TagMode};
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

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&local_git, &bare, &opts).expect("fetch_local from bare remote");
    resolve_ref(&local_git, "refs/remotes/origin/main").expect("tracking ref");
}
