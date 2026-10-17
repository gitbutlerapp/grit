//! Fetch must not overwrite an existing local tag when the remote tag moves.

use std::path::Path;
use std::process::Command;

use grit_lib::objects::ObjectId;
use grit_lib::refs::resolve_ref;
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn rev_parse(dir: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", rev]).trim()).expect("valid oid")
}

#[test]
fn fetch_local_rejects_moved_tag_without_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let remote = tmp.path().join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    std::fs::write(remote.join("a.txt"), b"a\n").unwrap();
    git(&remote, &["add", "a.txt"]);
    git(&remote, &["commit", "-q", "-m", "a"]);
    git(&remote, &["tag", "newtag"]);

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

    let tag_before = resolve_ref(&local_git, "refs/tags/newtag").expect("tag after first fetch");
    assert_eq!(tag_before, rev_parse(&remote, "refs/tags/newtag"));

    std::fs::write(remote.join("b.txt"), b"b\n").unwrap();
    git(&remote, &["add", "b.txt"]);
    git(&remote, &["commit", "-q", "-m", "b"]);
    git(&remote, &["tag", "-f", "newtag"]);

    let outcome = fetch_local(&local_git, &remote_git, &opts).expect("fetch after tag move");

    let tag_update = outcome
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/newtag")
        .expect("moved tag listed in library outcome");
    assert_eq!(tag_update.mode, UpdateMode::TagUpdateRejected);
    assert_eq!(tag_update.old_oid, Some(tag_before));
    assert_eq!(
        tag_update.new_oid,
        Some(rev_parse(&remote, "refs/tags/newtag"))
    );

    assert_eq!(
        resolve_ref(&local_git, "refs/tags/newtag").expect("local tag unchanged"),
        tag_before,
        "local tag must not move without force"
    );

    let outcome2 = fetch_local(&local_git, &remote_git, &opts).expect("repeat fetch");
    let repeat = outcome2
        .updates
        .iter()
        .find(|u| u.remote_ref == "refs/tags/newtag");
    assert!(
        repeat.is_none_or(|u| u.mode == UpdateMode::TagUpdateRejected),
        "repeat fetch must not claim the tag was applied"
    );

    let out = Command::new("git")
        .args(["rev-parse", "newtag"])
        .current_dir(&local)
        .output()
        .expect("git rev-parse");
    assert!(out.status.success());
    let git_tag =
        ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("git tag oid");
    assert_eq!(git_tag, tag_before);
}
