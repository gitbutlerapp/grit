//! Incremental local fetch with default `TagMode::Following`.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

#[test]
fn fetch_local_incremental_following_imports_reachable_tag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("README"), b"incremental tag\n").unwrap();
    git(&remote, &["add", "README"]);
    git(&remote, &["commit", "-q", "-m", "init"]);

    let local = tmp.path().join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main", "."]);
    let local_git = local.join(".git");
    let remote_git = remote.join(".git");

    let opts = FetchOptions {
        refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
        tags: TagMode::Following,
        ..Default::default()
    };
    fetch_local(&local_git, &remote_git, &opts).expect("initial fetch");

    std::fs::write(remote.join("next.txt"), b"second\n").unwrap();
    git(&remote, &["add", "next.txt"]);
    git(&remote, &["commit", "-q", "-m", "second"]);
    git(&remote, &["tag", "-a", "v2", "-m", "after second"]);

    let outcome = fetch_local(&local_git, &remote_git, &opts).expect("incremental fetch");

    let tag_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/v2")
        .expect("tag update in outcome");
    assert_eq!(
        tag_update.mode,
        UpdateMode::New,
        "incremental Following fetch must import newly reachable tag"
    );

    let tag_oid = rev_parse(&remote, "refs/tags/v2");
    assert_eq!(
        resolve_ref(&local_git, "refs/tags/v2").expect("tag ref"),
        tag_oid
    );

    let out = Command::new("git")
        .args(["cat-file", "-e", &tag_oid.to_hex()])
        .current_dir(&local)
        .env("GIT_DIR", &local_git)
        .output()
        .expect("cat-file");
    assert!(out.status.success(), "tag object must be present locally");
}
