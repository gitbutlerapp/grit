//! `grit push <path> <branch>` to a repository given by path (issue #2556): the push
//! exits 0 and creates no remote-tracking ref, whatever characters the path holds.
//! The remote is checked with the system `git`.

use grit_test_support::{git, grit_cmd};
use std::path::Path;

fn work_repo(root: &Path) -> std::path::PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("mkdir work");
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "hi\n").expect("write");
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "one"]);
    work
}

fn assert_pushed_without_tracking(work: &Path, bare: &Path, destination: &str) {
    let tip = git(work, &["rev-parse", "refs/heads/main"]);
    let out = grit_cmd(&["push", "--json", destination, "main"])
        .in_dir(work)
        .exec();
    assert!(out.ok(), "push to {destination}: {}", out.stderr);
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).expect("json");
    assert_eq!(value["rejected"], false, "{value}");
    assert_eq!(
        git(bare, &["rev-parse", "refs/heads/main"]).trim(),
        tip.trim(),
        "{destination}"
    );
    git(bare, &["fsck", "--strict"]);
    assert_eq!(
        git(work, &["for-each-ref", "refs/remotes/"]),
        "",
        "{destination}"
    );
}

#[test]
fn push_to_backslash_path_exits_zero_without_tracking_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = work_repo(tmp.path());
    let name = r"C:\t\remote.git";
    git(&work, &["init", "-q", "--bare", name]);
    assert_pushed_without_tracking(&work, &work.join(name), name);
}

#[test]
fn push_to_slash_free_relative_path_creates_no_tracking_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = work_repo(tmp.path());
    git(&work, &["init", "-q", "--bare", "remote.git"]);
    assert_pushed_without_tracking(&work, &work.join("remote.git"), "remote.git");
}

#[test]
fn push_to_absolute_path_and_file_url_exit_zero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = work_repo(tmp.path());
    let bare = tmp.path().join("remote.git");
    git(tmp.path(), &["init", "-q", "--bare", "remote.git"]);
    let path = bare.to_str().expect("utf8");
    assert_pushed_without_tracking(&work, &bare, path);
    assert_pushed_without_tracking(&work, &bare, &format!("file://{path}"));
}

#[test]
fn push_to_configured_remote_still_updates_tracking_ref() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = work_repo(tmp.path());
    let name = r"C:\t\remote.git";
    git(&work, &["init", "-q", "--bare", name]);
    git(&work, &["remote", "add", "origin", name]);
    let out = grit_cmd(&["push", "origin", "main"]).in_dir(&work).exec();
    assert!(out.ok(), "push origin: {}", out.stderr);
    assert_eq!(
        git(&work, &["rev-parse", "refs/remotes/origin/main"]),
        git(&work, &["rev-parse", "refs/heads/main"])
    );
}
