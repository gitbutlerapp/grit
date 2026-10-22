//! `grit branch` list flags vs system `git branch`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-branch-list-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run(dir: &Path, bin: &str, args: &[&str]) -> String {
    let out = Command::new(bin)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "{} {:?} failed: {}",
        bin,
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) -> String {
    run(dir, "git", args)
}

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let stdout = run(dir, GRIT, &full);
    serde_json::from_str(&stdout).expect("json")
}

fn branch_names_from_git(dir: &Path, git_args: &[&str]) -> Vec<String> {
    let out = git(dir, git_args);
    out.lines()
        .map(|line| line.trim_start_matches('*').trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

fn branch_names_from_grit_json(dir: &Path, grit_args: &[&str]) -> Vec<String> {
    let v = grit_json(dir, grit_args);
    assert_eq!(v["action"], "list");
    v["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_same_order(dir: &Path, git_args: &[&str], grit_args: &[&str]) -> TestResult {
    let git_names = branch_names_from_git(dir, git_args);
    let grit_names = branch_names_from_grit_json(dir, grit_args);
    assert_eq!(
        git_names, grit_names,
        "git {git_args:?} vs grit {grit_args:?}"
    );
    Ok(())
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@e.com"]);
    git(dir, &["config", "user.name", "T"]);
    fs::write(dir.join("f"), "x\n").unwrap();
    git(dir, &["add", "f"]);
    git(dir, &["commit", "-m", "one"]);
}

fn setup_with_origin(dir: &Path) {
    init_repo(dir);
    let bare = dir.parent().unwrap().join("origin.git");
    git(dir, &["init", "--bare", bare.to_str().unwrap()]);
    git(dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(dir, &["push", "-u", "origin", "main"]);
}

fn setup_rich_fixture(dir: &Path) {
    init_repo(dir);
    let bare = dir.parent().unwrap().join("remote.git");
    git(dir, &["init", "--bare", bare.to_str().unwrap()]);
    git(dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(dir, &["push", "-u", "origin", "main"]);
    git(dir, &["branch", "tracked"]);
    git(dir, &["push", "-u", "origin", "tracked"]);
    git(dir, &["checkout", "main"]);
    git(dir, &["branch", "gone-upstream"]);
    git(dir, &["config", "branch.gone-upstream.remote", "origin"]);
    git(
        dir,
        &[
            "config",
            "branch.gone-upstream.merge",
            "refs/heads/no-such-remote-branch",
        ],
    );
    git(
        dir,
        &[
            "update-ref",
            "-d",
            "refs/remotes/origin/no-such-remote-branch",
        ],
    );
    git(dir, &["branch", "wt-branch"]);
    git(
        dir,
        &[
            "worktree",
            "add",
            dir.parent().unwrap().join("wt").to_str().unwrap(),
            "wt-branch",
        ],
    );
    git(dir, &["checkout", "main"]);
}

#[test]
fn list_patterns_and_namespaces() -> TestResult {
    let scratch = Scratch::new("filters")?;
    let repo = scratch.child("repo");
    init_repo(&repo);
    git(&repo, &["branch", "feature"]);
    git(&repo, &["branch", "other"]);
    assert_same_order(
        &repo,
        &["branch", "--list", "feat*"],
        &["branch", "--list", "feat*"],
    )?;
    Ok(())
}

#[test]
fn list_remote_and_all_match_git_when_origin_exists() -> TestResult {
    let scratch = Scratch::new("remote-ns")?;
    let repo = scratch.child("repo");
    setup_with_origin(&repo);
    assert_same_order(&repo, &["branch", "-r"], &["branch", "-r"])?;
    assert_same_order(&repo, &["branch", "-a"], &["branch", "-a"])?;
    Ok(())
}

#[test]
fn local_origin_main_and_remote_tracking_do_not_share_current() -> TestResult {
    let scratch = Scratch::new("collision")?;
    let repo = scratch.child("repo");
    setup_with_origin(&repo);
    git(&repo, &["branch", "origin/main"]);
    git(&repo, &["checkout", "origin/main"]);
    let listed = grit_json(&repo, &["branch", "-a"]);
    let local = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["full_ref"] == "refs/heads/origin/main")
        .expect("local origin/main");
    let remote = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["full_ref"] == "refs/remotes/origin/main")
        .expect("remote origin/main");
    assert_eq!(local["current"], true);
    assert_eq!(remote["current"], false);
    assert!(remote.get("worktree").is_none());
    Ok(())
}

#[test]
fn list_merged_no_merged_contains() -> TestResult {
    let scratch = Scratch::new("reach")?;
    let repo = scratch.child("repo");
    init_repo(&repo);
    git(&repo, &["branch", "side"]);
    git(&repo, &["checkout", "side"]);
    fs::write(repo.join("f"), "y\n").unwrap();
    git(&repo, &["commit", "-am", "side"]);
    git(&repo, &["checkout", "main"]);
    assert_same_order(&repo, &["branch", "--merged"], &["branch", "--merged"])?;
    assert_same_order(
        &repo,
        &["branch", "--no-merged"],
        &["branch", "--no-merged"],
    )?;
    assert_same_order(
        &repo,
        &["branch", "--contains", "main"],
        &["branch", "--contains", "main"],
    )?;
    Ok(())
}

#[test]
fn list_sort_and_limit() -> TestResult {
    let scratch = Scratch::new("sort")?;
    let repo = scratch.child("repo");
    init_repo(&repo);
    for name in ["b", "a", "c"] {
        git(&repo, &["branch", name]);
    }
    assert_same_order(
        &repo,
        &["branch", "--sort=refname"],
        &["branch", "--sort", "name"],
    )?;
    let grit_names = branch_names_from_grit_json(&repo, &["branch", "-n", "2"]);
    assert_eq!(grit_names.len(), 2);
    Ok(())
}

#[test]
fn show_current_matches_git() -> TestResult {
    let scratch = Scratch::new("current")?;
    let repo = scratch.child("repo");
    init_repo(&repo);
    git(&repo, &["branch", "side"]);
    git(&repo, &["checkout", "side"]);
    let git_current = git(&repo, &["branch", "--show-current"]);
    let grit_current = run(&repo, GRIT, &["branch", "--show-current"]);
    assert_eq!(git_current.trim(), grit_current.trim());
    Ok(())
}

#[test]
fn json_fields_on_rich_fixture() -> TestResult {
    let scratch = Scratch::new("rich")?;
    let repo = scratch.child("repo");
    setup_rich_fixture(&repo);
    let listed = grit_json(&repo, &["branch"]);
    let tracked = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "tracked")
        .expect("tracked branch");
    assert!(tracked["full_ref"]
        .as_str()
        .unwrap()
        .starts_with("refs/heads/"));
    assert!(tracked["oid"].as_str().unwrap().len() >= 40);
    assert!(tracked["subject"].is_string());
    assert!(tracked["date"].is_string());
    assert!(tracked["upstream"].is_object());
    assert_eq!(tracked["upstream"]["ref"], "origin/tracked");
    let main = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "main")
        .expect("main");
    assert_eq!(main["upstream"]["ref"], "origin/main");
    let gone = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "gone-upstream")
        .expect("gone branch");
    assert_eq!(gone["upstream"]["gone"], Value::Bool(true));
    assert_eq!(gone["upstream"]["ref"], "origin/no-such-remote-branch");
    let wt = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "wt-branch")
        .expect("worktree branch");
    assert!(wt["worktree"].as_str().is_some());
    Ok(())
}

#[test]
fn upstream_honors_custom_fetch_refspec() -> TestResult {
    let scratch = Scratch::new("fetch-map")?;
    let repo = scratch.child("repo");
    init_repo(&repo);
    git(
        &repo,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/custom/*",
        ],
    );
    git(&repo, &["config", "branch.main.remote", "origin"]);
    git(&repo, &["config", "branch.main.merge", "refs/heads/main"]);
    let oid = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&repo, &["update-ref", &format!("refs/custom/main"), &oid]);
    let git_line = git(&repo, &["branch", "-vv"])
        .lines()
        .find(|l| l.contains('*'))
        .expect("current branch line")
        .to_owned();
    assert!(
        git_line.contains("[custom/main"),
        "git should show custom/main upstream: {git_line}"
    );
    assert!(
        !git_line.contains("[gone]"),
        "upstream should resolve: {git_line}"
    );
    let human = run(&repo, GRIT, &["branch"]);
    let grit_line = human
        .lines()
        .find(|l| l.starts_with('*'))
        .expect("current branch line");
    assert!(
        grit_line.contains("[custom/main"),
        "grit human: {grit_line}"
    );
    assert!(!grit_line.contains("[gone]"), "grit human: {grit_line}");
    let listed = grit_json(&repo, &["branch"]);
    let main = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == "main")
        .expect("main");
    assert_eq!(main["upstream"]["ref"], "custom/main");
    assert!(
        !main["upstream"]["gone"].as_bool().unwrap_or(false),
        "upstream should resolve: {:?}",
        main["upstream"]
    );
    Ok(())
}
