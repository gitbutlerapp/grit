//! Clone disk layout: pack retention, packed-refs, origin/HEAD, reflogs.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::repo::init_repository;
use grit_lib::transfer::{fetch_local, FetchOptions, TagMode};

fn git_in(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn clone_fetch_keeps_pack_and_matches_git_layout() {
    let upstream = tempfile::tempdir().expect("upstream");
    assert!(git_in(
        upstream.path(),
        &["init", "-q", "-b", "main", "."]
    ));
    std::fs::write(upstream.path().join("README"), b"clone layout test\n").unwrap();
    git_in(upstream.path(), &["add", "README"]);
    git_in(
        upstream.path(),
        &["-c", "user.email=t@e.com", "-c", "user.name=T", "commit", "-qm", "init"],
    );

    let clone = tempfile::tempdir().expect("clone");
    init_repository(clone.path(), false, "main", None, "files").expect("init");
    let clone_git = clone.path().join(".git");
    std::fs::OpenOptions::new()
        .append(true)
        .open(clone_git.join("config"))
        .expect("config")
        .write_all(b"\n[receive]\n\tunpacklimit = 0\n")
        .expect("write unpacklimit");

    fetch_local(
        &clone_git,
        &upstream.path().join(".git"),
        &FetchOptions {
            refspecs: vec!["+refs/heads/*:refs/remotes/origin/*".to_owned()],
            tags: TagMode::Following,
            initial_remote_fetch: true,
            remote_name: Some("origin".to_owned()),
            reflog_message: Some("clone: from file://fixture".to_owned()),
            ..Default::default()
        },
    )
    .expect("fetch");

    let count = git_out(&clone.path(), &["count-objects", "-v"]).expect("count-objects");
    assert!(
        count.contains("packs: 1"),
        "expected one pack, got:\n{count}"
    );
    assert!(
        count.contains("count: 0") || count.lines().any(|l| l.starts_with("count: 0")),
        "expected no loose objects, got:\n{count}"
    );

    assert!(clone_git.join("logs").exists(), "expected .git/logs");
    assert!(
        clone_git.join("packed-refs").exists(),
        "expected packed-refs"
    );
    assert!(
        clone_git.join("refs/remotes/origin/HEAD").exists(),
        "expected refs/remotes/origin/HEAD"
    );

    let fsck = Command::new("git")
        .current_dir(&clone.path())
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
