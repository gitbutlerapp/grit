//! `grit branch` create, delete, rename, upstream, and `--dry-run` vs system `git`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn init_repo() -> PathBuf {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "c2"]);
    std::mem::forget(tmp);
    root
}

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

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit")
}

fn json_ok(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<&str> = args.to_vec();
    full.push("--json");
    let out = grit(dir, &full);
    assert!(out.status.success(), "{:?}", out.stderr);
    serde_json::from_slice(&out.stdout).expect("json")
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "{:?}", out.stderr);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn snapshot_hash(dir: &Path) -> String {
    let config = Command::new("git")
        .args(["config", "--get-regexp", r"^branch\."])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    git_stdout(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ],
    ) + &config
        + &git_stdout(dir, &["reflog", "show", "--all", "--format=%gd %gs"])
}

#[test]
fn create_at_start_matches_git() {
    let root = init_repo();
    let start = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "main~1"])
            .current_dir(&root)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_owned();

    let v = json_ok(&root, &["branch", "topic", &start]);
    assert_eq!(v["action"], "create");
    assert_eq!(v["name"], "topic");
    assert_eq!(v["oid"].as_str().unwrap(), git_rev_parse(&root, "topic"));
    assert!(v
        .get("dry_run")
        .is_none_or(|d| !d.as_bool().unwrap_or(false)));

    git(&root, &["fsck", "--no-progress"]);
    assert_eq!(git_rev_parse(&root, "topic"), start);
    let log = git_reflog_first(&root, "topic");
    assert!(log.contains("branch:"));
}

#[test]
fn force_create_overwrites_tip() {
    let root = init_repo();
    assert!(grit(&root, &["branch", "topic"]).status.success());
    let before = git_rev_parse(&root, "topic");
    let v = json_ok(&root, &["branch", "topic", "main~1", "--force"]);
    assert_eq!(v["action"], "create");
    assert_eq!(
        git_rev_parse(&root, "topic"),
        git_rev_parse(&root, "main~1")
    );
    assert_ne!(before, git_rev_parse(&root, "topic"));
}

#[test]
fn delete_safe_and_force_delete_match_git() {
    let root = init_repo();
    git(&root, &["branch", "merged"]);
    git(&root, &["switch", "-q", "-c", "side"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "side"]);
    git(&root, &["switch", "-q", "main"]);

    let del = json_ok(&root, &["branch", "-d", "merged"]);
    assert_eq!(del["action"], "delete");
    assert_eq!(del["name"], "merged");
    assert!(!git_ref_exists(&root, "merged"));

    let err = grit(&root, &["branch", "-d", "side", "--json"]);
    assert!(!err.status.success());
    let err_json: serde_json::Value = serde_json::from_slice(&err.stdout).unwrap();
    assert_eq!(err_json["kind"], "branch_not_fully_merged");

    let force = json_ok(&root, &["branch", "-D", "side"]);
    assert_eq!(force["action"], "delete");
    assert!(!git_ref_exists(&root, "side"));
}

#[test]
fn delete_rejects_force_flag() {
    let root = init_repo();
    git(&root, &["branch", "ghost"]);
    let safe = grit(&root, &["branch", "-d", "ghost", "--force", "--json"]);
    assert_eq!(safe.status.code(), Some(2));
    let err: serde_json::Value = serde_json::from_slice(&safe.stdout).expect("json error");
    assert!(
        err["error"].as_str().unwrap_or("").contains("force-delete"),
        "usage error must mention --force-delete: {err}"
    );
    assert!(git_ref_exists(&root, "ghost"));

    git(&root, &["branch", "victim"]);
    let force = grit(&root, &["branch", "-D", "victim", "--force", "--json"]);
    assert_eq!(force.status.code(), Some(2));
    assert!(git_ref_exists(&root, "victim"));
}

#[test]
fn rename_and_force_move_match_git() {
    let root = init_repo();
    git(&root, &["branch", "old"]);
    let tip = git_rev_parse(&root, "old");

    let v = json_ok(&root, &["branch", "-m", "old", "new"]);
    assert_eq!(v["action"], "rename");
    assert_eq!(v["old"], "old");
    assert_eq!(v["new"], "new");
    assert_eq!(v["oid"].as_str().unwrap(), tip);
    assert!(!git_ref_exists(&root, "old"));
    assert_eq!(git_rev_parse(&root, "new"), tip);

    git(&root, &["branch", "other"]);
    git(&root, &["branch", "extra"]);
    let force_two = json_ok(&root, &["branch", "-M", "other", "extra"]);
    assert_eq!(force_two["action"], "rename");
    assert_eq!(force_two["old"], "other");
    assert_eq!(force_two["new"], "extra");
    assert!(!git_ref_exists(&root, "other"));
    assert_eq!(git_rev_parse(&root, "extra"), tip);

    git(&root, &["switch", "-q", "extra"]);
    let m = json_ok(&root, &["branch", "-M", "renamed"]);
    assert_eq!(m["action"], "rename");
    assert_eq!(m["new"], "renamed");
}

#[test]
fn set_and_unset_upstream_match_git_config() {
    let root = init_repo();
    git(&root, &["branch", "topic"]);
    git(&root, &["remote", "add", "origin", "/dev/null"]);
    let main_oid = git_rev_parse(&root, "main");
    git(
        &root,
        &["update-ref", "refs/remotes/origin/main", main_oid.as_str()],
    );

    let set = json_ok(&root, &["branch", "-u", "origin/main", "topic"]);
    assert_eq!(set["action"], "set_upstream");
    assert_eq!(set["branch"], "topic");
    assert_eq!(
        git_config(&root, "branch.topic.remote"),
        Some("origin".into())
    );
    assert_eq!(
        git_config(&root, "branch.topic.merge"),
        Some("refs/heads/main".into())
    );

    let unset = json_ok(&root, &["branch", "--unset-upstream", "topic"]);
    assert_eq!(unset["action"], "unset_upstream");
    assert_eq!(git_config(&root, "branch.topic.remote"), None);
}

#[test]
fn dry_run_leaves_refs_config_and_reflogs_unchanged() {
    let root = init_repo();
    git(&root, &["branch", "topic"]);
    git(&root, &["remote", "add", "origin", "/dev/null"]);
    let main_oid = git_rev_parse(&root, "main");
    git(
        &root,
        &["update-ref", "refs/remotes/origin/main", main_oid.as_str()],
    );
    let before = snapshot_hash(&root);

    let create = json_ok(&root, &["branch", "ghost", "--dry-run"]);
    assert_eq!(create["dry_run"], true);
    let delete = json_ok(&root, &["branch", "-d", "topic", "--dry-run"]);
    assert_eq!(delete["dry_run"], true);
    let rename = json_ok(&root, &["branch", "-m", "topic", "ghost2", "--dry-run"]);
    assert_eq!(rename["dry_run"], true);
    let upstream = json_ok(
        &root,
        &["branch", "-u", "origin/main", "topic", "--dry-run"],
    );
    assert_eq!(upstream["dry_run"], true);

    assert_eq!(before, snapshot_hash(&root));
    assert!(git_ref_exists(&root, "topic"));
    assert!(!git_ref_exists(&root, "ghost"));
}

#[test]
fn upstream_dry_run_validates_without_writing() {
    let root = init_repo();
    let before = snapshot_hash(&root);
    for args in [
        &[
            "branch",
            "-u",
            "missing/upstream",
            "ghost",
            "--dry-run",
            "--json",
        ][..],
        &["branch", "--unset-upstream", "ghost", "--dry-run", "--json"][..],
    ] {
        let out = grit(&root, args);
        assert!(!out.status.success(), "expected failure for {args:?}");
        let err: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(
            err.get("kind").is_some(),
            "structured error expected: {err}"
        );
    }
    assert_eq!(before, snapshot_hash(&root));
}

fn git_rev_parse(dir: &Path, rev: &str) -> String {
    String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir)
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_owned()
}

fn git_ref_exists(dir: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git_config(dir: &Path, key: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--get", key])
        .current_dir(dir)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        None
    }
}

fn git_reflog_first(dir: &Path, branch: &str) -> String {
    String::from_utf8_lossy(
        &Command::new("git")
            .args(["reflog", "show", "-1", branch])
            .current_dir(dir)
            .output()
            .unwrap()
            .stdout,
    )
    .to_string()
}
