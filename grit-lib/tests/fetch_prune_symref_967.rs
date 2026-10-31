//! Regression for GitHub issue #967: fetch --prune must not drop packed tracking
//! refs when pruning `refs/remotes/<remote>/HEAD`, and must not report phantom updates.

use std::fs;
use std::path::{Path, PathBuf};

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{Expected, RefUpdate};
use grit_lib::refs::{
    commit_ref_store_batch, pack_remote_tracking_refs_for_clone, resolve_ref, write_symbolic_ref,
};
use grit_lib::remote::transfer::{fetch_local, FetchOptions, TagMode, UpdateMode};
use grit_test_support::git;

fn setup_clone_like_layout(tmp: &Path) -> (PathBuf, PathBuf, ObjectId) {
    let remote = tmp.join("remote");
    let local = tmp.join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    fs::write(remote.join("a"), "a\n").unwrap();
    git(&remote, &["add", "a"]);
    git(&remote, &["commit", "-q", "-m", "a"]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    let remote_git = remote.join(".git");
    let local_git = local.join(".git");
    let main: ObjectId = git(&remote, &["rev-parse", "refs/heads/main"])
        .trim()
        .parse()
        .unwrap();
    // A clone holds the objects its remote-tracking refs name.
    git(
        &local,
        &["fetch", "-q", remote.to_str().unwrap(), "refs/heads/main"],
    );
    write_symbolic_ref(
        &local_git,
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    )
    .unwrap();
    write_ref_direct(&local_git, "refs/remotes/origin/main", &main);
    pack_remote_tracking_refs_for_clone(&local_git, "origin").unwrap();
    (local_git, remote_git, main)
}

fn write_ref_direct(git_dir: &Path, name: &str, oid: &ObjectId) {
    grit_lib::refs::write_ref(git_dir, name, oid).expect("write ref");
}

#[test]
fn batch_delete_origin_head_keeps_packed_main() {
    let tmp = tempfile::tempdir().unwrap();
    let (local_git, _, main) = setup_clone_like_layout(tmp.path());

    assert!(
        local_git.join("refs/remotes/origin/HEAD").is_file(),
        "HEAD symref should start loose"
    );
    let packed_before = fs::read_to_string(local_git.join("packed-refs")).unwrap();
    assert!(packed_before.contains("refs/remotes/origin/main"));

    commit_ref_store_batch(
        &local_git,
        &[RefUpdate {
            name: "refs/remotes/origin/HEAD".to_owned(),
            new_value: None,
            expected: Expected::Any,
            reflog: None,
            flags: grit_lib::refs::store::RefUpdateFlags {
                no_deref: true,
                ..Default::default()
            },
        }],
    )
    .expect("delete HEAD symref without peeling target");

    let packed_after = fs::read_to_string(local_git.join("packed-refs")).unwrap();
    assert!(
        packed_after.contains("refs/remotes/origin/main"),
        "packed main must survive HEAD delete\nbefore:\n{packed_before}\nafter:\n{packed_after}"
    );
    assert_eq!(
        resolve_ref(&local_git, "refs/remotes/origin/main").unwrap(),
        main
    );
}

fn setup_slash_remote_layout(tmp: &Path) -> (PathBuf, PathBuf) {
    let remote = tmp.join("remote");
    let local = tmp.join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::create_dir_all(&local).unwrap();
    git(&remote, &["init", "-q", "-b", "main", "."]);
    fs::write(remote.join("a"), "a\n").unwrap();
    git(&remote, &["add", "a"]);
    git(&remote, &["commit", "-q", "-m", "a"]);
    git(&local, &["init", "-q", "-b", "main", "."]);
    let remote_git = remote.join(".git");
    let local_git = local.join(".git");
    let main: ObjectId = git(&remote, &["rev-parse", "refs/heads/main"])
        .trim()
        .parse()
        .unwrap();
    // A clone holds the objects its remote-tracking refs name.
    git(
        &local,
        &["fetch", "-q", remote.to_str().unwrap(), "refs/heads/main"],
    );
    write_symbolic_ref(
        &local_git,
        "refs/remotes/team/origin/HEAD",
        "refs/remotes/team/origin/main",
    )
    .unwrap();
    write_ref_direct(&local_git, "refs/remotes/team/origin/main", &main);
    pack_remote_tracking_refs_for_clone(&local_git, "team/origin").unwrap();
    (local_git, remote_git)
}

#[test]
fn fetch_local_prune_slash_remote_name_no_phantom_head_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let (local_git, remote_git) = setup_slash_remote_layout(tmp.path());

    let opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/team/origin/*".to_owned()];
        o.remote_name = Some("team/origin".to_owned());
        o.tags = TagMode::Following;
        o.prune = true;
        o
    };
    for pass in 1..=2 {
        let outcome = fetch_local(&local_git, &remote_git, &opts).expect("fetch");
        let noisy: Vec<_> = outcome
            .updates
            .iter()
            .filter(|u| u.mode != UpdateMode::UpToDate)
            .collect();
        assert!(
            noisy.is_empty(),
            "pass {pass}: prune fetch must not report phantom updates for team/origin: {noisy:?}"
        );
    }
    assert!(
        local_git.join("refs/remotes/team/origin/HEAD").is_file(),
        "remote HEAD symref must remain"
    );
}

#[test]
fn fetch_local_prune_twice_reports_no_updates_when_up_to_date() {
    let tmp = tempfile::tempdir().unwrap();
    let (local_git, remote_git, _) = setup_clone_like_layout(tmp.path());

    let opts = {
        let mut o = FetchOptions::default();
        o.refspecs = vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()];
        o.tags = TagMode::Following;
        o.prune = true;
        o
    };
    let first = fetch_local(&local_git, &remote_git, &opts).expect("first fetch");
    let noisy: Vec<_> = first
        .updates
        .iter()
        .filter(|u| u.mode != UpdateMode::UpToDate)
        .collect();
    assert!(
        noisy.is_empty(),
        "first prune fetch on up-to-date clone layout should report no changes: {noisy:?}"
    );

    let second = fetch_local(&local_git, &remote_git, &opts).expect("second fetch");
    let noisy: Vec<_> = second
        .updates
        .iter()
        .filter(|u| u.mode != UpdateMode::UpToDate)
        .collect();
    assert!(
        noisy.is_empty(),
        "second prune fetch should report no changes: {noisy:?}"
    );
}
