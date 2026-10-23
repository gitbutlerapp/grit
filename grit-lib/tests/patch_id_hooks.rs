//! Patch-ID and hooks library integration tests (task #1479).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::patch_ids::PatchIdMode;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_patch_ids_from_log(dir: &std::path::Path) -> Vec<(ObjectId, ObjectId)> {
    let log_p = git_in(dir, &["log", "-p", "--reverse"]);
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["patch-id", "--stable"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("git patch-id");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(log_p.as_bytes())
        .expect("write log");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.split_whitespace();
            let patch = ObjectId::from_hex(parts.next().expect("patch")).expect("patch hex");
            let commit = ObjectId::from_hex(parts.next().expect("commit")).expect("commit hex");
            (patch, commit)
        })
        .collect()
}

#[test]
fn patch_id_matches_git_patch_id_stable() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "first"]);
    std::fs::write(dir.path().join("f.txt"), b"one\ntwo\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "second"]);
    std::fs::write(dir.path().join("g.txt"), b"new\n").unwrap();
    git_in(dir.path(), &["add", "g.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "third"]);
    std::fs::write(dir.path().join("bin.dat"), &[0u8, 1, 2, 3]).unwrap();
    git_in(dir.path(), &["add", "bin.dat"]);
    git_in(dir.path(), &["commit", "-q", "-m", "binary"]);
    std::fs::write(dir.path().join("h.txt"), b"a\n").unwrap();
    git_in(dir.path(), &["add", "h.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-h"]);
    git_in(dir.path(), &["mv", "h.txt", "i.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "rename"]);
    let mut perms = std::fs::metadata(dir.path().join("f.txt"))
        .expect("f.txt")
        .permissions();
    perms.set_mode(0o100755);
    std::fs::set_permissions(dir.path().join("f.txt"), perms).expect("chmod");
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "mode-only"]);
    std::fs::write(dir.path().join("j.txt"), b"j\n").unwrap();
    git_in(dir.path(), &["add", "j.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-j"]);
    std::fs::write(dir.path().join("k.txt"), b"k\n").unwrap();
    git_in(dir.path(), &["add", "k.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-k"]);
    std::fs::write(dir.path().join("l.txt"), b"l\n").unwrap();
    git_in(dir.path(), &["add", "l.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "add-l"]);

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let git_ids = git_patch_ids_from_log(dir.path());
    assert!(
        git_ids.len() >= 10,
        "expected at least 10 patch-ids, got {}",
        git_ids.len()
    );

    for (git_patch, commit) in git_ids {
        let grit = repo
            .patch_id(&commit, PatchIdMode::Stable)
            .expect("patch_id")
            .expect("non-merge commit should have patch-id");
        assert_eq!(grit, git_patch, "patch-id for {}", commit.to_hex());
    }
}

fn git_patch_id_from_show(dir: &std::path::Path, flag: &str) -> ObjectId {
    let show_p = git_in(dir, &["show", "-p", "HEAD"]);
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["patch-id", flag])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git patch-id");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(show_p.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git patch-id {flag}");
    ObjectId::from_hex(
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .expect("patch id"),
    )
    .expect("hex")
}

#[test]
fn patch_id_verbatim_matches_git_whitespace_sensitive() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f.txt"), b"one\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.path().join("f.txt"), b"one  \ntwo\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "trailing-space"]);

    let commit: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let git_stable = git_patch_id_from_show(dir.path(), "--stable");
    let git_verbatim = git_patch_id_from_show(dir.path(), "--verbatim");
    assert_ne!(git_stable, git_verbatim, "fixture must differ by mode");

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let grit_stable = repo
        .patch_id(&commit, PatchIdMode::Stable)
        .expect("stable")
        .expect("some");
    let grit_verbatim = repo
        .patch_id(&commit, PatchIdMode::Verbatim)
        .expect("verbatim")
        .expect("some");

    assert_eq!(grit_stable, git_stable, "stable ODB vs git");
    assert_eq!(grit_verbatim, git_verbatim, "verbatim ODB vs git");
    assert_ne!(grit_stable, grit_verbatim);
}

#[test]
fn patch_id_stable_and_unstable_differ_for_multi_file_commit() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git_in(dir.path(), &["add", "a.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.path().join("a.txt"), b"a\nchanged\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b\n").unwrap();
    git_in(dir.path(), &["add", "a.txt", "b.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "two-file"]);

    let commit: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let git_stable_id = git_patch_id_from_show(dir.path(), "--stable");
    let git_unstable_id = git_patch_id_from_show(dir.path(), "--unstable");
    assert_ne!(
        git_stable_id, git_unstable_id,
        "git stable and unstable must differ for this fixture"
    );

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");

    let grit_stable = repo
        .patch_id(&commit, PatchIdMode::Stable)
        .expect("stable")
        .expect("some");
    let grit_unstable = repo
        .patch_id(&commit, PatchIdMode::Unstable)
        .expect("unstable")
        .expect("some");

    assert_eq!(
        grit_stable, git_stable_id,
        "stable ODB vs git patch-id --stable"
    );
    assert_eq!(
        grit_unstable, git_unstable_id,
        "unstable ODB vs git patch-id --unstable"
    );
    assert_ne!(grit_stable, grit_unstable);
}

#[test]
fn merge_commit_has_no_patch_id() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "--initial-branch=main", "."]);
    std::fs::write(dir.path().join("f.txt"), b"x\n").unwrap();
    git_in(dir.path(), &["add", "f.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "base"]);
    git_in(dir.path(), &["checkout", "-q", "-b", "side"]);
    std::fs::write(dir.path().join("side.txt"), b"s\n").unwrap();
    git_in(dir.path(), &["add", "side.txt"]);
    git_in(dir.path(), &["commit", "-q", "-m", "side"]);
    git_in(dir.path(), &["checkout", "-q", "main"]);
    git_in(dir.path(), &["merge", "-q", "--no-ff", "side"]);
    let merge_tip: ObjectId = git_in(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    assert!(repo
        .patch_id(&merge_tip, PatchIdMode::Stable)
        .expect("read")
        .is_none());
}

#[test]
fn hooks_list_respects_core_hooks_path() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "."]);
    let custom = dir.path().join("my-hooks");
    std::fs::create_dir_all(&custom).expect("mkdir");
    let hook_path = custom.join("pre-commit");
    std::fs::write(&hook_path, b"#!/bin/sh\nexit 0\n").expect("write hook");
    let mut perms = std::fs::metadata(&hook_path).expect("meta").permissions();
    perms.set_mode(0o100755);
    std::fs::set_permissions(&hook_path, perms).expect("chmod");
    git_in(
        dir.path(),
        &["config", "core.hooksPath", custom.to_str().expect("utf8")],
    );

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let hooks = repo.hooks().list(None).expect("list").items;
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].name, "pre-commit");
    assert_eq!(hooks[0].path, hook_path);
    assert!(hooks[0].executable);
    assert_eq!(
        hooks[0].source,
        grit_lib::repo_handles::HookSource::CoreHooksPath
    );
}

#[test]
fn non_executable_hook_reported() {
    let dir = TempDir::new().expect("tempdir");
    git_in(dir.path(), &["init", "-q", "."]);
    let hooks_dir = dir.path().join("custom-hooks");
    std::fs::create_dir_all(&hooks_dir).expect("mkdir");
    git_in(
        dir.path(),
        &[
            "config",
            "core.hooksPath",
            hooks_dir.to_str().expect("utf8"),
        ],
    );
    let hook_path = hooks_dir.join("pre-push");
    std::fs::write(&hook_path, b"#!/bin/sh\nexit 0\n").expect("write");
    std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o100644)).expect("chmod");

    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open");
    let hooks = repo.hooks().list(None).expect("list").items;
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].name, "pre-push");
    assert!(!hooks[0].executable);
    assert_eq!(
        hooks[0].source,
        grit_lib::repo_handles::HookSource::CoreHooksPath
    );
}
