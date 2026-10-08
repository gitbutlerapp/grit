//! Regression: `fetch_local` over a bare repo with an empty commit must ingest
//! the built pack without checksum errors (parallel index-pack included).

use std::path::Path;
use std::process::Command;

use grit_lib::transfer::{fetch_local, FetchOptions, TagMode};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fetch_local_empty_commit_bare_remote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).expect("workdir");
    git(&work, &["init", "-q", "-b", "main", "."]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    git(&work, &["branch", "topic"]);
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
    git(
        &consumer,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&consumer.join(".git"), &bare, &opts).expect("fetch_local");
}
