//! `grit branch --remote` / `--all` / `-n` vs system `git`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "{:?}", out.stderr);
}

fn grit_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<&str> = args.to_vec();
    full.push("--json");
    let out = Command::new(GS)
        .args(&full)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(out.status.success(), "{:?}", out.stderr);
    serde_json::from_slice(&out.stdout).expect("json")
}

fn remote_tracking_branch_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = names
        .into_iter()
        .filter(|name| name.contains('/') && !name.ends_with("/HEAD"))
        .collect();
    out.sort();
    out
}

fn git_remote_branch_names(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/remotes/"])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let names = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned);
    remote_tracking_branch_names(names)
}

struct RemoteListFixture {
    _root: tempfile::TempDir,
    clone: PathBuf,
}

fn clone_with_remotes() -> RemoteListFixture {
    let root = tempfile::tempdir().expect("tempdir");
    let origin = root.path().join("origin");
    std::fs::create_dir_all(&origin).expect("mkdir");
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(&origin, &["branch", "-q", "topic"]);
    git(&origin, &["checkout", "-q", "main"]);
    let clone = root.path().join("clone");
    git(
        root.path(),
        &["clone", "-q", "origin", clone.to_str().unwrap()],
    );
    git(&clone, &["fetch", "-q", "origin"]);
    RemoteListFixture { _root: root, clone }
}

#[test]
fn branch_remote_json_matches_git_for_each_ref() {
    let fixture = clone_with_remotes();
    let repo = &fixture.clone;
    let expected = git_remote_branch_names(repo);
    assert!(
        expected.len() >= 2,
        "fixture should have main and topic on origin"
    );

    let listed = grit_json(repo, &["branch", "--remote"]);
    assert_eq!(listed["action"], "list");
    let names: Vec<String> = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            assert_eq!(b["is_remote"], true);
            assert!(b["oid"].as_str().unwrap().len() >= 40);
            assert_eq!(b["short_oid"].as_str().unwrap().len(), 7);
            b["name"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(
        remote_tracking_branch_names(names),
        remote_tracking_branch_names(expected)
    );
}

#[test]
fn branch_limit_caps_remote_list() {
    let fixture = clone_with_remotes();
    let listed = grit_json(&fixture.clone, &["branch", "--remote", "-n", "1"]);
    assert_eq!(listed["branches"].as_array().unwrap().len(), 1);
}

#[test]
fn branch_remote_empty_list_message() {
    let root = tempfile::tempdir().expect("tempdir");
    git(root.path(), &["init", "-q", "-b", "main"]);
    git(
        root.path(),
        &["commit", "-q", "--allow-empty", "-m", "seed"],
    );
    let out = Command::new(GS)
        .args(["branch", "--remote"])
        .current_dir(root.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(out.status.success(), "{:?}", out.stderr);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "No remote-tracking branches."
    );
}
