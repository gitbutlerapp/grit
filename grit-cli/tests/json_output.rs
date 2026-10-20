//! Integration tests for the global `--json` output mode.
//!
//! These invoke the compiled `gs` binary and parse its stdout as JSON, asserting
//! both the data and — for the key commands — the exact top-level key set, so the
//! schema is a stable, drift-proof contract.

// Integration tests favor readability; allow the panicky helpers the rest of the
// workspace forbids in library code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn diff_json_line_text(line: &Value) -> String {
    if let Some(text) = line.get("text").and_then(|v| v.as_str()) {
        return text.to_owned();
    }
    line.get("segments")
        .and_then(|v| v.as_array())
        .map(|segments| {
            segments
                .iter()
                .filter_map(|seg| seg.get("text").and_then(|t| t.as_str()))
                .collect::<String>()
        })
        .unwrap_or_default()
}

struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn dump(&self) -> String {
        format!(
            "exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-cli-json-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
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

fn gs<I, S>(dir: &Path, args: I) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let out = Command::new(GS)
        .args(&args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn gs");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Run a plain (human) command, asserting success.
fn gs_ok(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = gs(dir, args);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    out
}

/// Run `gs --json <args>`, assert success, and parse stdout as a JSON object.
fn gs_json(dir: &Path, args: &[&str]) -> Value {
    let mut full: Vec<&str> = vec!["--json"];
    full.extend_from_slice(args);
    let out = gs(dir, &full);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let value: Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}):\n{}", out.dump()));
    assert!(value.is_object(), "expected a JSON object:\n{}", out.dump());
    value
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn system_git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn git");
    assert_eq!(
        out.status.code(),
        Some(0),
        "git {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn path_arg(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

fn write_file(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write file");
}

/// Sorted top-level key names of a JSON object (for schema-stability assertions).
fn keys(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Per-command happy paths
// ---------------------------------------------------------------------------

#[test]
fn init_emits_json() -> TestResult {
    let scratch = Scratch::new("init")?;
    let v = gs_json(scratch.path(), &["init", "."]);
    assert_eq!(v["initialized"], Value::Bool(true));
    assert_eq!(v["reinitialized"], Value::Null);
    assert_eq!(v["bare"], Value::Bool(false));
    assert_eq!(v["branch"], "main");
    assert!(
        v["path"].as_str().unwrap().ends_with(".git"),
        "path should be the .git dir: {v}"
    );

    let again = gs_json(scratch.path(), &["init", "."]);
    assert_eq!(again["initialized"], Value::Bool(false));
    assert_eq!(again["reinitialized"], Value::Bool(true));
    Ok(())
}

#[test]
fn status_json_reports_untracked_staged_and_clean() -> TestResult {
    let scratch = Scratch::new("status")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    // Untracked.
    write_file(&repo.join("a.txt"), "hi\n");
    let v = gs_json(&repo, &["status"]);
    assert_eq!(v["branch"], "main");
    assert_eq!(v["clean"], Value::Bool(false));
    assert_eq!(v["head"], Value::Null);
    assert_eq!(v["untracked"], serde_json::json!(["a.txt"]));
    assert_eq!(v["staged"].as_array().unwrap().len(), 0);

    // Staged.
    gs_ok(&repo, &["add"]);
    let v = gs_json(&repo, &["status"]);
    let staged = v["staged"].as_array().unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0]["path"], "a.txt");
    assert_eq!(staged[0]["status"], "added");

    // Clean after commit.
    gs_ok(&repo, &["commit", "first"]);
    let v = gs_json(&repo, &["status"]);
    assert_eq!(v["clean"], Value::Bool(true));
    assert!(v["head"].as_str().is_some(), "head oid after commit: {v}");
    Ok(())
}

#[test]
fn status_json_merge_conflict_lists_path_once_with_merge_fields() -> TestResult {
    let scratch = Scratch::new("status-merge-conflict")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    system_git_ok(&repo, &["init", "-q", "-b", "main"]);
    system_git_ok(&repo, &["config", "user.name", "T"]);
    system_git_ok(&repo, &["config", "user.email", "t@e.com"]);
    write_file(&repo.join("f"), "base\n");
    system_git_ok(&repo, &["add", "f"]);
    system_git_ok(&repo, &["commit", "-qm", "base"]);
    system_git_ok(&repo, &["checkout", "-qb", "side"]);
    write_file(&repo.join("f"), "side\n");
    system_git_ok(&repo, &["commit", "-qam", "side"]);
    system_git_ok(&repo, &["checkout", "-q", "main"]);
    write_file(&repo.join("f"), "main\n");
    system_git_ok(&repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git merge");
    assert_ne!(
        merge.status.code(),
        Some(0),
        "expected merge conflict:\n{}",
        String::from_utf8_lossy(&merge.stderr)
    );

    let v = gs_json(&repo, &["status"]);
    assert_eq!(v["merging"], Value::Bool(true));
    assert_eq!(v["in_progress"], serde_json::json!(["merge"]));
    assert_eq!(v["conflicts"], serde_json::json!(["f"]));
    assert_eq!(v["staged"].as_array().unwrap().len(), 1);
    assert_eq!(v["staged"][0]["path"], "f");
    assert_eq!(v["staged"][0]["status"], "unmerged");
    assert_eq!(v["unstaged"].as_array().unwrap().len(), 0);
    assert!(
        !v["untracked"].as_array().unwrap().iter().any(|p| p == "f"),
        "conflicted path must not be untracked: {v}"
    );
    assert_eq!(v["clean"], Value::Bool(false));
    Ok(())
}

#[test]
fn status_json_merge_conflict_keeps_ahead_of_target() -> TestResult {
    let scratch = Scratch::new("status-merge-ahead")?;
    let remote = scratch.child("origin.git");
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(scratch.path(), &["init", "--bare", &path_arg(&remote)]);
    system_git_ok(&repo, &["init", "-q", "-b", "main"]);
    system_git_ok(&repo, &["config", "user.name", "T"]);
    system_git_ok(&repo, &["config", "user.email", "t@e.com"]);
    system_git_ok(&repo, &["remote", "add", "origin", &path_arg(&remote)]);
    write_file(&repo.join("f"), "base\n");
    system_git_ok(&repo, &["add", "f"]);
    system_git_ok(&repo, &["commit", "-qm", "base"]);
    system_git_ok(&repo, &["push", "-u", "origin", "main"]);
    system_git_ok(&repo, &["checkout", "-qb", "side"]);
    write_file(&repo.join("f"), "side\n");
    system_git_ok(&repo, &["commit", "-qam", "side"]);
    system_git_ok(&repo, &["checkout", "-q", "main"]);
    write_file(&repo.join("f"), "main\n");
    system_git_ok(&repo, &["commit", "-qam", "main on main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git merge");
    assert_ne!(merge.status.code(), Some(0), "expected merge conflict");

    let v = gs_json(&repo, &["status"]);
    assert_eq!(v["merging"], Value::Bool(true));
    assert_eq!(v["target"], "origin/main");
    assert_eq!(v["ahead"], 1);
    assert_eq!(v["behind"], 0);
    assert_eq!(v["commits"].as_array().unwrap().len(), 1);
    assert_eq!(v["conflicts"], serde_json::json!(["f"]));
    Ok(())
}

fn git_symmetric_ahead_behind(repo: &Path) -> (usize, usize) {
    let out = Command::new("git")
        .args(["rev-list", "--left-right", "--count", "HEAD...origin/main"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git rev-list");
    assert!(
        out.status.success(),
        "git rev-list failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let ahead = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let behind = parts.next().unwrap_or("0").parse().unwrap_or(0);
    (ahead, behind)
}

#[test]
fn status_json_uses_upstream_after_switch_from_remote_tracking() -> TestResult {
    let scratch = Scratch::new("status-switch-upstream")?;
    let remote = scratch.child("r.git");
    let upstream = scratch.child("w");
    let clone = scratch.child("c");
    fs::create_dir_all(&upstream)?;
    system_git_ok(
        scratch.path(),
        &["init", "-q", "--bare", &path_arg(&remote)],
    );
    system_git_ok(&upstream, &["clone", "-q", &path_arg(&remote), "."]);
    system_git_ok(&upstream, &["config", "user.name", "T"]);
    system_git_ok(&upstream, &["config", "user.email", "t@e.com"]);
    write_file(&upstream.join("f"), "1\n");
    system_git_ok(&upstream, &["add", "f"]);
    system_git_ok(&upstream, &["commit", "-qm", "main"]);
    system_git_ok(&upstream, &["push", "-u", "origin", "HEAD:main"]);
    system_git_ok(&upstream, &["checkout", "-qb", "topic"]);
    write_file(&upstream.join("g"), "2\n");
    system_git_ok(&upstream, &["add", "g"]);
    system_git_ok(&upstream, &["commit", "-qm", "topic"]);
    system_git_ok(&upstream, &["push", "-u", "origin", "HEAD:topic"]);

    system_git_ok(
        scratch.path(),
        &[
            "clone",
            "-q",
            "-b",
            "main",
            &path_arg(&remote),
            &path_arg(&clone),
        ],
    );

    gs_ok(&clone, &["switch", "topic"]);

    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["branch"], "topic");
    assert_eq!(v["target"], "origin/topic");
    assert_eq!(v["ahead"], 0);
    assert_eq!(v["behind"], 0);
    assert_eq!(v["clean"], Value::Bool(true));

    let git_remote = Command::new("git")
        .args(["config", "--get", "branch.topic.remote"])
        .current_dir(&clone)
        .output()
        .expect("git config");
    assert!(git_remote.status.success());
    assert_eq!(String::from_utf8_lossy(&git_remote.stdout).trim(), "origin");
    Ok(())
}

#[test]
fn status_json_behind_and_diverged_match_git() -> TestResult {
    let scratch = Scratch::new("status-behind")?;
    let remote = scratch.child("r.git");
    let upstream = scratch.child("w");
    let clone = scratch.child("c");
    fs::create_dir_all(&upstream)?;
    system_git_ok(
        scratch.path(),
        &["init", "-q", "--bare", &path_arg(&remote)],
    );
    system_git_ok(&upstream, &["clone", "-q", &path_arg(&remote), "."]);
    system_git_ok(&upstream, &["config", "user.name", "T"]);
    system_git_ok(&upstream, &["config", "user.email", "t@e.com"]);
    write_file(&upstream.join("one.txt"), "1\n");
    system_git_ok(&upstream, &["add", "one.txt"]);
    system_git_ok(&upstream, &["commit", "-qm", "one"]);
    system_git_ok(&upstream, &["push", "-u", "origin", "HEAD:main"]);

    system_git_ok(
        scratch.path(),
        &[
            "clone",
            "-q",
            "-b",
            "main",
            &path_arg(&remote),
            &path_arg(&clone),
        ],
    );

    write_file(&upstream.join("two.txt"), "2\n");
    system_git_ok(&upstream, &["add", "two.txt"]);
    system_git_ok(&upstream, &["commit", "-qm", "two"]);
    system_git_ok(&upstream, &["push", "origin", "HEAD:main"]);

    gs_ok(&clone, &["fetch"]);

    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["target"], "origin/main");
    let (git_ahead, git_behind) = git_symmetric_ahead_behind(&clone);
    assert_eq!(git_ahead, 0);
    assert_eq!(git_behind, 1);
    assert_eq!(v["ahead"], git_ahead);
    assert_eq!(v["behind"], git_behind);
    assert_eq!(v["clean"], Value::Bool(true));

    let human = gs_ok(&clone, &["status"]);
    assert!(
        human.stdout.contains("1 behind origin/main"),
        "human behind:\n{}",
        human.stdout
    );
    assert!(
        human
            .stdout
            .contains("grit pull to update from origin/main"),
        "pull hint:\n{}",
        human.stdout
    );

    write_file(&clone.join("local.txt"), "local\n");
    gs_ok(&clone, &["add", "local.txt"]);
    gs_ok(&clone, &["commit", "-m", "local"]);

    let v = gs_json(&clone, &["status"]);
    let (git_ahead, git_behind) = git_symmetric_ahead_behind(&clone);
    assert_eq!(git_ahead, 1);
    assert_eq!(git_behind, 1);
    assert_eq!(v["ahead"], git_ahead);
    assert_eq!(v["behind"], git_behind);

    let human = gs_ok(&clone, &["status"]);
    assert!(
        human.stdout.contains("1 ahead, 1 behind origin/main"),
        "human diverged:\n{}",
        human.stdout
    );
    Ok(())
}

#[test]
fn add_and_commit_emit_json() -> TestResult {
    let scratch = Scratch::new("commit")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "hi\n");

    let added = gs_json(&repo, &["add"]);
    assert_eq!(added["staged"], 1);

    let committed = gs_json(&repo, &["commit", "first commit"]);
    assert_eq!(committed["branch"], "main");
    assert_eq!(committed["subject"], "first commit");
    assert_eq!(committed["changes"], 1);
    assert_eq!(committed["oid"].as_str().unwrap().len(), 40);
    assert_eq!(committed["amended"], Value::Bool(false));
    Ok(())
}

#[test]
fn commit_amend_emits_json() -> TestResult {
    let scratch = Scratch::new("commit-amend")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "v1\n");
    gs_ok(&repo, &["commit", "seed"]);

    write_file(&repo.join("a.txt"), "v2\n");
    let amended = gs_json(&repo, &["commit", "--amend", "revised"]);
    assert_eq!(amended["amended"], Value::Bool(true));
    assert_eq!(amended["subject"], "revised");
    assert_eq!(amended["changes"], 1);
    Ok(())
}

#[test]
fn log_json_pages_with_next_cursor() -> TestResult {
    let scratch = Scratch::new("log")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    // 11 commits → one full page (10) plus a `next` cursor.
    for i in 0..11 {
        write_file(&repo.join("a.txt"), &format!("v{i}\n"));
        gs_ok(&repo, &["add", "a.txt"]);
        gs_ok(&repo, &["commit", &format!("commit {i}")]);
    }
    let v = gs_json(&repo, &["log"]);
    assert_eq!(v["commits"].as_array().unwrap().len(), 10);
    assert!(v["next"].as_str().is_some(), "expected a next cursor: {v}");
    let first = &v["commits"][0];
    assert_eq!(first["subject"], "commit 10");
    assert_eq!(first["oid"].as_str().unwrap().len(), 40);

    // Last page: follow `next`, expect no further cursor.
    let next = v["next"].as_str().unwrap().to_owned();
    assert!(
        next.starts_with("lw1."),
        "next must be an opaque walk cursor, got {next}"
    );
    let v2 = gs_json(&repo, &["log", &format!("--cursor={next}")]);
    assert_eq!(v2["next"], Value::Null);
    Ok(())
}

#[test]
fn log_json_paging_through_merge_matches_git_order() -> TestResult {
    let scratch = Scratch::new("log-merge-page")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "base\n");
    gs_ok(&repo, &["add", "a.txt"]);
    gs_ok(&repo, &["commit", "base"]);

    gs_ok(&repo, &["branch", "side"]);
    write_file(&repo.join("a.txt"), "side\n");
    gs_ok(&repo, &["add", "a.txt"]);
    gs_ok(&repo, &["commit", "side"]);

    gs_ok(&repo, &["switch", "main"]);
    write_file(&repo.join("a.txt"), "main\n");
    gs_ok(&repo, &["add", "a.txt"]);
    gs_ok(&repo, &["commit", "main"]);

    gs_ok(&repo, &["merge", "side"]);

    let git_log = Command::new("git")
        .args(["-C"])
        .arg(&repo)
        .args(["log", "--format=%H"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git log");
    assert!(git_log.status.success());
    let git_oids: Vec<String> = String::from_utf8_lossy(&git_log.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();

    let mut paged: Vec<String> = Vec::new();
    let mut next: Option<String> = None;
    for _ in 0..20 {
        let v = if let Some(ref cursor) = next {
            gs_json(&repo, &["log", &format!("--cursor={cursor}")])
        } else {
            gs_json(&repo, &["log"])
        };
        for c in v["commits"].as_array().expect("commits") {
            paged.push(c["oid"].as_str().expect("oid").to_owned());
        }
        next = v["next"].as_str().map(str::to_owned);
        if next.is_none() {
            break;
        }
    }
    assert_eq!(paged, git_oids);
    Ok(())
}

#[test]
fn branch_json_list_create_delete() -> TestResult {
    let scratch = Scratch::new("branch")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "hi\n");
    gs_ok(&repo, &["commit", "first"]);

    let created = gs_json(&repo, &["branch", "topic"]);
    assert_eq!(created["action"], "create");
    assert_eq!(created["name"], "topic");

    let listed = gs_json(&repo, &["branch"]);
    assert_eq!(listed["action"], "list");
    assert_eq!(listed["current"], "main");
    let names: Vec<&str> = listed["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["main", "topic"]);
    let main = &listed["branches"][0];
    assert_eq!(main["current"], Value::Bool(true));

    let deleted = gs_json(&repo, &["branch", "-d", "topic"]);
    assert_eq!(deleted["action"], "delete");
    assert_eq!(deleted["name"], "topic");
    assert!(deleted["oid"].as_str().unwrap().len() >= 40);
    assert_eq!(deleted["short_oid"].as_str().unwrap().len(), 7);
    Ok(())
}

#[test]
fn switch_json() -> TestResult {
    let scratch = Scratch::new("switch")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "hi\n");
    gs_ok(&repo, &["commit", "first"]);

    let created = gs_json(&repo, &["switch", "-c", "topic"]);
    assert_eq!(created["branch"], "topic");
    assert_eq!(created["created"], Value::Bool(true));
    assert_eq!(created["detached"], Value::Bool(false));

    let switched = gs_json(&repo, &["switch", "main"]);
    assert_eq!(switched["branch"], "main");
    assert_eq!(switched["created"], Value::Bool(false));
    assert_eq!(switched["detached"], Value::Bool(false));
    Ok(())
}

#[test]
fn remote_json_list_and_add() -> TestResult {
    let scratch = Scratch::new("remote")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    let empty = gs_json(&repo, &["remote"]);
    assert_eq!(empty["action"], "list");
    assert_eq!(empty["remotes"].as_array().unwrap().len(), 0);

    let added = gs_json(
        &repo,
        &["remote", "add", "origin", "https://example.com/r.git"],
    );
    assert_eq!(added["action"], "add");
    assert_eq!(added["name"], "origin");
    assert_eq!(added["url"], "https://example.com/r.git");

    let listed = gs_json(&repo, &["remote"]);
    assert_eq!(listed["remotes"][0]["name"], "origin");
    assert_eq!(listed["remotes"][0]["url"], "https://example.com/r.git");
    Ok(())
}

#[test]
fn config_json_get_list_set_unset() -> TestResult {
    let scratch = Scratch::new("config")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    let set = gs_json(&repo, &["config", "user.name", "A Developer"]);
    assert_eq!(set["action"], "set");
    assert_eq!(set["key"], "user.name");
    assert_eq!(set["value"], "A Developer");

    let got = gs_json(&repo, &["config", "user.name"]);
    assert_eq!(got["value"], "A Developer");

    let listed = gs_json(&repo, &["config", "--list"]);
    assert_eq!(listed["action"], "list");
    let has = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["key"] == "user.name" && e["value"] == "A Developer");
    assert!(has, "user.name should appear in --list: {listed}");

    let unset = gs_json(&repo, &["config", "--unset", "user.name"]);
    assert_eq!(unset["action"], "unset");
    assert_eq!(unset["key"], "user.name");
    Ok(())
}

#[test]
fn shortlog_json() -> TestResult {
    let scratch = Scratch::new("shortlog")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "hi\n");
    gs_ok(&repo, &["commit", "first"]);

    let v = gs_json(&repo, &["shortlog"]);
    assert_eq!(v["branch"], "main");
    // No remote/target configured in a fresh repo beyond local `main`.
    assert!(v["commits"].is_array(), "commits is an array: {v}");
    assert!(v["ahead"].is_number(), "ahead is a number: {v}");
    Ok(())
}

#[test]
fn clone_json_bundle_reports_transfer_stats() -> TestResult {
    let scratch = Scratch::new("clone-bundle-stats")?;
    let seed = scratch.child("seed");
    fs::create_dir_all(&seed)?;
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main", "."])
        .current_dir(&seed)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?
        .success());
    write_file(&seed.join("f.txt"), "x\n");
    assert!(Command::new("git")
        .args(["add", "f.txt"])
        .current_dir(&seed)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@e.com",
            "commit",
            "-qm",
            "c",
        ])
        .current_dir(&seed)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?
        .success());
    let bundle = scratch.path().join("repo.bundle");
    assert!(Command::new("git")
        .args([
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/main",
        ])
        .current_dir(&seed)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?
        .success());
    let clone = scratch.child("cloned");
    let cloned = gs_json(
        scratch.path(),
        &["clone", &path_arg(&bundle), &path_arg(&clone)],
    );
    assert!(cloned["objects"].as_u64().unwrap_or(0) > 0);
    assert!(cloned["bytes"].as_u64().unwrap_or(0) > 0);
    Ok(())
}

#[test]
fn clone_json_has_stats_fields() -> TestResult {
    let scratch = Scratch::new("clone-stats")?;
    let seed = scratch.child("seed");
    let remote = scratch.child("remote.git");
    let clone = scratch.child("clone");
    fs::create_dir_all(&seed)?;

    gs_ok(&seed, &["init", "."]);
    write_file(&seed.join("README.md"), "seed\n");
    gs_ok(&seed, &["commit", "seed commit"]);
    gs_ok(&seed, &["tag", "v0"]);
    gs_ok(scratch.path(), &["init", "--bare", &path_arg(&remote)]);
    gs_ok(&seed, &["remote", "add", "origin", &path_arg(&remote)]);
    gs_ok(&seed, &["push"]);

    let cloned = gs_json(
        scratch.path(),
        &["clone", &path_arg(&remote), &path_arg(&clone)],
    );
    for key in ["objects", "bytes", "branches", "tags", "files"] {
        assert!(
            cloned.get(key).is_some(),
            "clone json missing {key}: {cloned}"
        );
        assert!(
            cloned[key].is_number(),
            "clone json {key} must be a number: {cloned}"
        );
    }
    assert!(cloned["objects"].as_u64().unwrap_or(0) > 0);
    assert!(cloned["bytes"].as_u64().unwrap_or(0) > 0);
    assert!(cloned["branches"].as_u64().unwrap_or(0) >= 1);
    assert!(cloned["files"].as_u64().unwrap_or(0) >= 1);
    assert!(
        cloned.get("elapsed_ms").is_none() && cloned.get("elapsed").is_none(),
        "elapsed must not appear in json: {cloned}"
    );
    Ok(())
}

#[test]
fn remote_workflow_json_clone_push_fetch_pull() -> TestResult {
    let scratch = Scratch::new("workflow")?;
    let seed = scratch.child("seed");
    let remote = scratch.child("remote.git");
    let clone = scratch.child("clone");
    fs::create_dir_all(&seed)?;

    gs_ok(&seed, &["init", "."]);
    write_file(&seed.join("README.md"), "seed\n");
    gs_ok(&seed, &["commit", "seed commit"]);

    gs_ok(scratch.path(), &["init", "--bare", &path_arg(&remote)]);
    gs_ok(&seed, &["remote", "add", "origin", &path_arg(&remote)]);

    // Push.
    let pushed = gs_json(&seed, &["push"]);
    assert_eq!(pushed["remote"], "origin");
    assert_eq!(pushed["rejected"], Value::Bool(false));
    assert_eq!(pushed["results"][0]["status"], "ok");

    // Clone.
    let cloned = gs_json(
        scratch.path(),
        &["clone", &path_arg(&remote), &path_arg(&clone)],
    );
    assert_eq!(cloned["branch"], "main");
    assert_eq!(cloned["path"], path_arg(&clone));

    // New commit in the clone, push it back.
    write_file(&clone.join("clone.txt"), "from clone\n");
    gs_ok(&clone, &["commit", "-am", "clone work"]);
    gs_ok(&clone, &["push"]);

    // Fetch sees the update.
    let fetched = gs_json(&seed, &["fetch"]);
    assert_eq!(fetched["remote"], "origin");
    assert_eq!(fetched["updated"], 1);
    assert_eq!(fetched["updates"][0]["ref"], "refs/remotes/origin/main");

    // Pull fast-forwards.
    let pulled = gs_json(&seed, &["pull"]);
    assert_eq!(pulled["result"], "fast_forward");
    assert!(pulled["oid"].as_str().is_some(), "ff oid: {pulled}");
    Ok(())
}

#[test]
fn diff_json_uncommitted_and_commit() -> TestResult {
    let scratch = Scratch::new("diff")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "one\ntwo\nthree\n");
    gs_ok(&repo, &["commit", "first"]);

    // No changes yet → empty file list.
    let clean = gs_json(&repo, &["diff"]);
    assert_eq!(clean["files"].as_array().unwrap().len(), 0);

    // Uncommitted change: one line modified.
    write_file(&repo.join("a.txt"), "one\nTWO\nthree\n");
    let v = gs_json(&repo, &["diff"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "a.txt");
    assert_eq!(files[0]["status"], "modified");
    let lines = files[0]["hunks"][0]["lines"].as_array().unwrap();
    // The modified line shows as a del (old line 2) and an add (new line 2).
    assert!(lines.iter().any(|l| l["kind"] == "del" && l["old"] == 2));
    assert!(lines.iter().any(|l| l["kind"] == "add" && l["new"] == 2));

    // Commit diff: the change a specific commit introduced (root commit → all added).
    let head = gs_json(&repo, &["log"])["commits"][0]["oid"]
        .as_str()
        .unwrap()
        .to_owned();
    let cv = gs_json(&repo, &["diff", &head]);
    let cfiles = cv["files"].as_array().unwrap();
    assert_eq!(cfiles[0]["path"], "a.txt");
    assert!(cfiles[0]["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["kind"] == "add"));
    for line in cfiles[0]["hunks"][0]["lines"].as_array().unwrap() {
        assert!(
            line.get("text").is_some() && line.get("segments").is_none(),
            "line-level JSON must use compact text field: {line}"
        );
    }
    Ok(())
}

#[test]
fn diff_json_payload_stays_near_line_text_size() -> TestResult {
    let scratch = Scratch::new("diff-json-size")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    let mut body = String::new();
    for i in 0..400 {
        body.push_str(&format!("line {i}\n"));
    }
    write_file(&repo.join("big.txt"), &body);
    gs_ok(&repo, &["add", "big.txt"]);
    gs_ok(&repo, &["commit", "v1"]);
    write_file(&repo.join("big.txt"), &body.replace("line", "LINE"));
    let out = gs_ok(&repo, &["--json", "diff"]);
    let json_bytes = out.stdout.len();
    let line_text_bytes: usize = serde_json::from_str::<Value>(&out.stdout)?["files"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| f["hunks"].as_array().unwrap())
        .flat_map(|h| h["lines"].as_array().unwrap())
        .map(|l| diff_json_line_text(l).len())
        .sum();
    assert!(
        line_text_bytes > 5000,
        "fixture should materialize many diff lines: {line_text_bytes}"
    );
    assert!(
        json_bytes <= line_text_bytes.saturating_mul(6),
        "JSON overhead too high: json={json_bytes} line_text={line_text_bytes}"
    );
    assert!(
        !out.stdout.contains("\"segments\""),
        "line-level diff JSON must not emit segments arrays"
    );
    assert!(
        !out.stdout.contains('\n') || out.stdout.matches('\n').count() <= 1,
        "diff --json should be compact (at most one trailing newline)"
    );
    Ok(())
}

#[test]
fn diff_json_head_includes_staged_added_then_worktree_edited() -> TestResult {
    let scratch = Scratch::new("diff-staged-add-edit")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("base.txt"), "base\n");
    gs_ok(&repo, &["commit", "init"]);
    write_file(&repo.join("new.txt"), "staged\n");
    gs_ok(&repo, &["add", "new.txt"]);
    write_file(&repo.join("new.txt"), "worktree\n");

    let git = Command::new("git")
        .args(["diff", "HEAD", "--numstat"])
        .current_dir(&repo)
        .output()?;
    assert!(
        git.status.success(),
        "git diff: {}",
        String::from_utf8_lossy(&git.stderr)
    );
    let numstat = String::from_utf8_lossy(&git.stdout);
    assert!(
        numstat.contains("new.txt"),
        "git must report new.txt vs HEAD: {numstat}"
    );

    let v = gs_json(&repo, &["diff"]);
    let files = v["files"].as_array().unwrap();
    let newf = files.iter().find(|f| f["path"] == "new.txt").unwrap();
    assert_eq!(newf["status"], "added");
    let kinds: Vec<_> = newf["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.iter().any(|k| *k == "add"),
        "expected add lines for worktree content: {kinds:?}"
    );
    assert!(
        newf["hunks"][0]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["kind"] == "add" && diff_json_line_text(l) == "worktree"),
        "new side must reflect current worktree bytes"
    );
    Ok(())
}

#[test]
fn diff_json_unstaged_eol_crlf_normalizes_worktree_lines() -> TestResult {
    let scratch = Scratch::new("diff-eol990")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git")
            .success());
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "a"]);
    git(&["config", "user.email", "a@b"]);
    fs::write(repo.join(".gitattributes"), "*.txt text eol=crlf\n")?;
    fs::write(repo.join("f.txt"), "a\nb\nc\n")?;
    git(&["add", "-A"]);
    git(&["commit", "-qm", "1"]);
    fs::remove_file(repo.join("f.txt"))?;
    git(&["checkout", "--", "f.txt"]);
    fs::write(repo.join("f.txt"), b"a\r\nB\r\nc\r\n")?;

    let v = gs_json(&repo, &["diff", "--unstaged"]);
    let kinds: Vec<_> = v["files"][0]["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["context", "del", "add", "context"],
        "one line change after eol normalization: {kinds:?}"
    );
    for line in v["files"][0]["hunks"][0]["lines"].as_array().unwrap() {
        let text = diff_json_line_text(line);
        assert!(
            !text.contains('\r'),
            "JSON line text must not retain CR: {text:?}"
        );
    }
    Ok(())
}

#[test]
fn diff_json_preserves_crlf_no_eof_and_mode() -> TestResult {
    let scratch = Scratch::new("diffmeta")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    std::fs::write(repo.join("crlf.txt"), b"a\r\nb\r\n")?;
    std::fs::write(repo.join("nonl.txt"), b"line1\nline2")?;
    std::fs::write(repo.join("run.sh"), b"x\n")?;
    gs_ok(&repo, &["commit", "init"]);

    std::fs::write(repo.join("crlf.txt"), b"a\nb\n")?;
    std::fs::write(repo.join("nonl.txt"), b"line1\nline2\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(repo.join("run.sh"))?.permissions();
        perms.set_mode(0o100755);
        fs::set_permissions(repo.join("run.sh"), perms)?;
    }

    let v = gs_json(&repo, &["diff"]);
    let files = v["files"].as_array().unwrap();
    let crlf = files.iter().find(|f| f["path"] == "crlf.txt").unwrap();
    let del = crlf["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["kind"] == "del")
        .unwrap();
    assert_eq!(diff_json_line_text(del), "a\r");

    let nonl = files.iter().find(|f| f["path"] == "nonl.txt").unwrap();
    let del2 = nonl["hunks"][0]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["kind"] == "del" && l["old"] == 2)
        .unwrap();
    assert_eq!(del2["no_newline_at_eof"], true);

    let run = files.iter().find(|f| f["path"] == "run.sh").unwrap();
    assert_eq!(run["old_mode"], "100644");
    assert_eq!(run["new_mode"], "100755");

    let h = &crlf["hunks"][0];
    assert!(h.get("old_lines").is_some());
    assert!(h.get("new_lines").is_some());
    Ok(())
}

#[cfg(unix)]
fn gs_in_pseudo_tty(dir: &Path, args: &[&str]) -> CmdOutput {
    let mut cmd = format!("cd {} && env -u NO_COLOR {}", dir.display(), GS);
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&shell_escape(arg));
    }
    let out = Command::new("script")
        .args(["-qfc", &cmd, "/dev/null"])
        .output()
        .expect("spawn script");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[cfg(unix)]
fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{s}'")
    }
}

#[test]
#[cfg(unix)]
fn diff_color_tty_shows_hunk_range_without_function_context() -> TestResult {
    let scratch = Scratch::new("difftty")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("top.txt"), "first\nsecond\n");
    gs_ok(&repo, &["commit", "init"]);
    write_file(&repo.join("top.txt"), "FIRST\nsecond\n");
    let out = gs_in_pseudo_tty(&repo, &["diff"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stdout.contains('\u{1b}'),
        "expected ANSI color in PTY output"
    );
    assert!(
        out.stdout.contains("@@ -") && out.stdout.contains(" +"),
        "expected counted hunk header in PTY output: {}",
        out.dump()
    );
    Ok(())
}

#[test]
fn diff_human_deleted_labels_deleted_before_mode() -> TestResult {
    let scratch = Scratch::new("diffdelhuman")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    std::fs::write(repo.join("gone.txt"), b"x\n")?;
    gs_ok(&repo, &["commit", "init"]);
    fs::remove_file(repo.join("gone.txt"))?;
    let out = gs(&repo, ["diff"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stdout.contains("deleted"),
        "expected deleted marker in human diff: {}",
        out.dump()
    );
    assert!(
        out.stdout.contains("old mode 100644"),
        "expected old mode line: {}",
        out.dump()
    );
    Ok(())
}

#[test]
#[cfg(unix)]
fn diff_binary_mode_only_shows_modes_not_content_message() -> TestResult {
    let scratch = Scratch::new("diffbinmode")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    std::fs::write(repo.join("bin.dat"), b"a\0b\n")?;
    gs_ok(&repo, &["commit", "init"]);
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(repo.join("bin.dat"))?.permissions();
    perms.set_mode(0o100755);
    fs::set_permissions(repo.join("bin.dat"), perms)?;
    let out = gs(&repo, ["diff"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(out.stdout.contains("old mode 100644"), "{}", out.dump());
    assert!(out.stdout.contains("new mode 100755"), "{}", out.dump());
    assert!(
        !out.stdout.contains("Binary file differs"),
        "mode-only binary must not claim content diff: {}",
        out.dump()
    );
    Ok(())
}

#[test]
fn diff_binary_content_shows_differs_message() -> TestResult {
    let scratch = Scratch::new("diffbincontent")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    std::fs::write(repo.join("bin.dat"), b"old\0bytes")?;
    gs_ok(&repo, &["commit", "init"]);
    std::fs::write(repo.join("bin.dat"), b"new\0bytes")?;
    let out = gs(&repo, ["diff"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stdout.contains("Binary file differs"),
        "binary content change must be noted: {}",
        out.dump()
    );
    Ok(())
}

fn assert_binary_json_no_hunk_content(v: &Value, label: &str) {
    let files = v["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1, "{label}: {v}");
    let file = &files[0];
    assert_eq!(file["binary"], true, "{label}: {file}");
    let hunks = file["hunks"].as_array().expect("hunks");
    assert!(
        hunks.is_empty(),
        "{label}: expected no hunks, got {hunks:?}"
    );
}

#[test]
fn diff_unstaged_binary_json_omits_hunks_small_and_large() -> TestResult {
    for (tag, size) in [("small", 2000_usize), ("large", 200_000_usize)] {
        let scratch = Scratch::new(&format!("diffbinjson-{tag}"))?;
        let repo = scratch.child("repo");
        fs::create_dir_all(&repo)?;
        gs_ok(&repo, &["init", "."]);
        let mut initial = vec![0_u8];
        initial.extend(std::iter::repeat_n(0xAB_u8, size.saturating_sub(1)));
        std::fs::write(repo.join("img.bin"), &initial)?;
        gs_ok(&repo, &["commit", "init"]);
        let mut updated = vec![0_u8];
        updated.extend(std::iter::repeat_n(0xCD_u8, size.saturating_sub(1)));
        std::fs::write(repo.join("img.bin"), &updated)?;
        let v = gs_json(&repo, &["diff", "--unstaged"]);
        assert_binary_json_no_hunk_content(&v, tag);
    }
    Ok(())
}

#[test]
fn diff_unstaged_binary_json_omits_hunks_when_only_index_side_is_binary() -> TestResult {
    let scratch = Scratch::new("diffbinjson-mixed")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    std::fs::write(repo.join("img.bin"), b"a\0committed")?;
    gs_ok(&repo, &["commit", "init"]);
    std::fs::write(repo.join("img.bin"), &[0xFF, 0xFE, 0xFD])?;
    let v = gs_json(&repo, &["diff", "--unstaged"]);
    assert_binary_json_no_hunk_content(&v, "mixed sides");
    Ok(())
}

#[test]
fn diff_two_commits_and_range() -> TestResult {
    let scratch = Scratch::new("difftwocommit")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "v1\n");
    gs_ok(&repo, &["commit", "c1"]);
    write_file(&repo.join("a.txt"), "v2\n");
    gs_ok(&repo, &["commit", "c2"]);

    for args in [
        &["diff", "HEAD~1", "HEAD"][..],
        &["diff", "HEAD~1..HEAD"][..],
    ] {
        let v = gs_json(&repo, args);
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "args {args:?}: {v}");
        assert_eq!(files[0]["path"], "a.txt");
    }
    Ok(())
}

#[test]
fn diff_staged_only() -> TestResult {
    let scratch = Scratch::new("diffstaged")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("staged.txt"), "a\n");
    write_file(&repo.join("unstaged.txt"), "b\n");
    gs_ok(&repo, &["commit", "init"]);
    write_file(&repo.join("staged.txt"), "A\n");
    write_file(&repo.join("unstaged.txt"), "B\n");
    gs_ok(&repo, &["add", "staged.txt"]);

    let all = gs_json(&repo, &["diff"]);
    assert_eq!(all["files"].as_array().unwrap().len(), 2);

    let staged = gs_json(&repo, &["diff", "--staged"]);
    let sfiles = staged["files"].as_array().unwrap();
    assert_eq!(sfiles.len(), 1);
    assert_eq!(sfiles[0]["path"], "staged.txt");

    let unstaged = gs_json(&repo, &["diff", "--unstaged"]);
    let ufiles = unstaged["files"].as_array().unwrap();
    assert_eq!(ufiles.len(), 1);
    assert_eq!(ufiles[0]["path"], "unstaged.txt");
    Ok(())
}

#[test]
fn diff_commit_range_detects_rename() -> TestResult {
    let scratch = Scratch::new("diffrename")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "line\n");
    gs_ok(&repo, &["commit", "one"]);
    fs::create_dir_all(&repo.join("d"))?;
    gs_ok(&repo, &["mv", "a.txt", "d/a.txt"]);
    gs_ok(&repo, &["commit", "moved"]);

    let v = gs_json(&repo, &["diff", "HEAD~1", "HEAD"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{v}");
    assert_eq!(files[0]["status"], "renamed");
    assert_eq!(files[0]["path"], "d/a.txt");
    assert_eq!(files[0]["old_path"], "a.txt");
    Ok(())
}

#[test]
fn diff_staged_detects_rename() -> TestResult {
    let scratch = Scratch::new("diffstagedrename")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    let body: String = (1..=40)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&repo.join("a.txt"), &format!("{body}\n"));
    gs_ok(&repo, &["commit", "one"]);
    gs_ok(&repo, &["mv", "a.txt", "b.txt"]);

    let v = gs_json(&repo, &["diff", "--staged"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{v}");
    assert_eq!(files[0]["status"], "renamed");
    assert_eq!(files[0]["path"], "b.txt");
    assert_eq!(files[0]["old_path"], "a.txt");
    Ok(())
}

#[test]
fn diff_plain_mv_detects_rename_unstaged_and_default() -> TestResult {
    let scratch = Scratch::new("diffplainmv")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    let body: String = (1..=40)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&repo.join("a.txt"), &format!("{body}\n"));
    gs_ok(&repo, &["commit", "one"]);
    fs::rename(repo.join("a.txt"), repo.join("b.txt"))?;

    for args in [&["diff", "--unstaged"][..], &["diff"][..]] {
        let v = gs_json(&repo, args);
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "args {args:?}: {v}");
        assert_eq!(files[0]["status"], "renamed");
        assert_eq!(files[0]["path"], "b.txt");
        assert_eq!(files[0]["old_path"], "a.txt");
    }
    Ok(())
}

#[test]
fn diff_unstaged_omits_unrelated_untracked_when_tracked_deleted() -> TestResult {
    let scratch = Scratch::new("diffdeluntracked")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("tracked.txt"), "body\n");
    gs_ok(&repo, &["commit", "one"]);
    fs::remove_file(repo.join("tracked.txt"))?;
    write_file(&repo.join("untracked.txt"), "noise\n");

    let v = gs_json(&repo, &["diff", "--unstaged"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{v}");
    assert_eq!(files[0]["status"], "deleted");
    assert_eq!(files[0]["path"], "tracked.txt");
    Ok(())
}

#[test]
fn diff_worktree_rename_with_small_unstaged_edit() -> TestResult {
    let scratch = Scratch::new("diffworktreerename")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    let body: String = (1..=40)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    write_file(&repo.join("a.txt"), &format!("{body}\n"));
    gs_ok(&repo, &["commit", "one"]);
    gs_ok(&repo, &["mv", "a.txt", "b.txt"]);
    write_file(&repo.join("b.txt"), &format!("{body}\n41\n"));

    let v = gs_json(&repo, &["diff"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{v}");
    assert_eq!(files[0]["status"], "renamed");
    assert_eq!(files[0]["path"], "b.txt");
    assert_eq!(files[0]["old_path"], "a.txt");
    Ok(())
}

#[test]
fn diff_against_ref_compares_worktree() -> TestResult {
    let scratch = Scratch::new("diffagainst")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("f.txt"), "base\n");
    gs_ok(&repo, &["commit", "base"]);
    write_file(&repo.join("f.txt"), "tip\n");
    gs_ok(&repo, &["commit", "tip"]);
    write_file(&repo.join("f.txt"), "worktree\n");

    let v = gs_json(&repo, &["diff", "--against", "HEAD~1"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "f.txt");
    Ok(())
}

fn git_annotated_tag(repo: &Path, name: &str) {
    system_git_ok(
        repo,
        &[
            "-c",
            "user.email=t@e.com",
            "-c",
            "user.name=T",
            "tag",
            "-a",
            name,
            "-m",
            name,
        ],
    );
}

#[test]
fn diff_two_annotated_tags() -> TestResult {
    let scratch = Scratch::new("difftags")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "one\n");
    gs_ok(&repo, &["commit", "c1"]);
    git_annotated_tag(&repo, "v1");
    write_file(&repo.join("a.txt"), "two\n");
    gs_ok(&repo, &["commit", "c2"]);
    git_annotated_tag(&repo, "v2");

    let v = gs_json(&repo, &["diff", "v1", "v2"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "a.txt");

    let range = gs_json(&repo, &["diff", "v1..v2"]);
    assert_eq!(range["files"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn diff_against_annotated_tag() -> TestResult {
    let scratch = Scratch::new("diffagainsttag")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("f.txt"), "tagged\n");
    gs_ok(&repo, &["commit", "c1"]);
    git_annotated_tag(&repo, "v1");
    write_file(&repo.join("f.txt"), "worktree\n");

    let v = gs_json(&repo, &["diff", "--against", "v1"]);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "f.txt");
    Ok(())
}

#[test]
fn diff_human_shows_hunk_range_with_function_context() -> TestResult {
    let scratch = Scratch::new("difffctx")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("f.rs"), "fn foo() {\n  a\n}\n");
    gs_ok(&repo, &["commit", "first"]);
    write_file(&repo.join("f.rs"), "fn foo() {\n  b\n}\n");
    let out = gs(&repo, ["diff"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stdout.contains("@@ -") && out.stdout.contains("fn foo()"),
        "expected counted hunk header with function context: {}",
        out.dump()
    );
    assert!(
        out.stdout.contains(",3") || out.stdout.contains("-1,3"),
        "expected line counts in header: {}",
        out.dump()
    );
    Ok(())
}

#[test]
fn diff_human_is_plain_when_piped() -> TestResult {
    let scratch = Scratch::new("diffhuman")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "alpha\n");
    gs_ok(&repo, &["commit", "first"]);
    write_file(&repo.join("a.txt"), "beta\n");

    let out = gs(&repo, ["diff"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    // Piped → no ANSI escapes, and the file header + both sides are present.
    assert!(!out.stdout.contains('\u{1b}'), "piped diff must be plain");
    assert!(out.stdout.contains("a.txt"), "{}", out.dump());
    assert!(out.stdout.contains("- alpha"), "{}", out.dump());
    assert!(out.stdout.contains("+ beta"), "{}", out.dump());
    Ok(())
}

#[test]
fn show_json_commit_branch_and_tag() -> TestResult {
    let scratch = Scratch::new("show")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "one\n");
    gs_ok(&repo, &["commit", "first"]);
    write_file(&repo.join("a.txt"), "one\ntwo\n");
    gs_ok(&repo, &["commit", "second"]);

    // Default (HEAD on a branch) → branch kind, with the commit and its diff.
    let v = gs_json(&repo, &["show"]);
    assert_eq!(v["kind"], "branch");
    assert_eq!(v["ref_name"], "main");
    assert_eq!(v["commit"]["subject"], "second");
    assert_eq!(v["commit"]["oid"].as_str().unwrap().len(), 40);
    assert_eq!(v["commit"]["author"]["email"], "test@example.com");
    // `show` carries a diffstat, not the full patch.
    assert_eq!(v["stat"]["files"][0]["path"], "a.txt");
    assert_eq!(v["stat"]["files_changed"], 1);
    assert_eq!(v["stat"]["insertions"], 1);

    // By commit sha → commit kind.
    let sha = v["commit"]["oid"].as_str().unwrap().to_owned();
    let c = gs_json(&repo, &["show", &sha]);
    assert_eq!(c["kind"], "commit");
    assert_eq!(c["commit"]["oid"], sha);

    // Lightweight tag (a ref pointing straight at a commit) → tag kind.
    let tags = repo.join(".git/refs/tags");
    fs::create_dir_all(&tags)?;
    fs::write(tags.join("v1"), format!("{sha}\n"))?;
    let t = gs_json(&repo, &["show", "v1"]);
    assert_eq!(t["kind"], "tag");
    assert_eq!(t["ref_name"], "v1");
    assert_eq!(t["commit"]["oid"], sha);
    Ok(())
}

#[test]
fn show_human_has_commit_header_and_diff() -> TestResult {
    let scratch = Scratch::new("showhuman")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "hello\n");
    gs_ok(&repo, &["commit", "the subject"]);

    let out = gs(&repo, ["show"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(!out.stdout.contains('\u{1b}'), "piped show must be plain");
    assert!(out.stdout.contains("commit "), "{}", out.dump());
    assert!(out.stdout.contains("Author: Test User <test@example.com>"));
    assert!(out.stdout.contains("    the subject"));
    assert!(out.stdout.contains("a.txt"));
    Ok(())
}

#[test]
fn rfc3339_author_date_matches_git_for_negative_subhour_tz() -> TestResult {
    const WHEN: &str = "1700000000 -0030";
    let scratch = Scratch::new("tz-negative-subhour")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "x\n");
    gs_ok(&repo, &["add"]);

    let commit_out = Command::new(GS)
        .current_dir(&repo)
        .args(["commit", "tz test"])
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", WHEN)
        .env("GIT_COMMITTER_DATE", WHEN)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn gs");
    assert!(
        commit_out.status.success(),
        "commit failed: {}",
        String::from_utf8_lossy(&commit_out.stderr)
    );

    let git_iso = Command::new("git")
        .current_dir(&repo)
        .args(["log", "-1", "--format=%aI"])
        .output()
        .expect("git log");
    assert!(git_iso.status.success());
    let expected = String::from_utf8_lossy(&git_iso.stdout).trim().to_owned();
    assert_eq!(expected, "2023-11-14T21:43:20-00:30");

    let log = gs_json(&repo, &["log"]);
    assert_eq!(log["commits"][0]["author"]["date"], expected);

    let show = gs_json(&repo, &["show"]);
    assert_eq!(show["commit"]["author"]["date"], expected);
    assert_eq!(show["commit"]["committer"]["date"], expected);
    Ok(())
}

#[test]
fn log_show_blame_json_author_objects_match() -> TestResult {
    let scratch = Scratch::new("author-shape")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "line one\n");
    gs_ok(&repo, &["commit", "seed"]);

    let show_author = gs_json(&repo, &["show"])["commit"]["author"].clone();
    let log_author = gs_json(&repo, &["log"])["commits"][0]["author"].clone();
    assert_eq!(log_author, show_author);

    let blame = gs_json(&repo, &["blame", "a.txt"]);
    let show = gs_json(&repo, &["show"]);
    let head = show["commit"]["oid"].as_str().expect("oid");
    let blame_author = &blame["commits"][head]["author"];
    assert_eq!(*blame_author, show_author);
    Ok(())
}

#[test]
fn log_json_includes_author_and_rfc3339_date() -> TestResult {
    let scratch = Scratch::new("log-fields")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "x\n");
    gs_ok(&repo, &["commit", "hello"]);

    let v = gs_json(&repo, &["log"]);
    let commit = &v["commits"][0];
    assert_eq!(commit["subject"], "hello");
    assert_eq!(commit["author"]["name"], "Test User");
    assert_eq!(commit["author"]["email"], "test@example.com");
    assert!(commit["author"]["date"]
        .as_str()
        .is_some_and(|d| d.contains('T')));
    assert!(commit["relative_date"].as_str().is_some());
    Ok(())
}

#[test]
fn log_json_honors_limit_without_cursor() -> TestResult {
    let scratch = Scratch::new("log-limit")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    for i in 0..8 {
        write_file(&repo.join("a.txt"), &format!("v{i}\n"));
        gs_ok(&repo, &["add", "a.txt"]);
        gs_ok(&repo, &["commit", &format!("commit {i}")]);
    }
    let v = gs_json(&repo, &["log", "--limit", "5"]);
    assert_eq!(v["commits"].as_array().unwrap().len(), 5);
    assert_eq!(v["next"], Value::Null);
    assert_eq!(v["commits"][0]["subject"], "commit 7");
    Ok(())
}

#[test]
fn log_json_filters_by_path() -> TestResult {
    let scratch = Scratch::new("log-path")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("keep.txt"), "a\n");
    write_file(&repo.join("skip.txt"), "b\n");
    gs_ok(&repo, &["add", "."]);
    gs_ok(&repo, &["commit", "both"]);
    write_file(&repo.join("keep.txt"), "a2\n");
    gs_ok(&repo, &["add", "keep.txt"]);
    gs_ok(&repo, &["commit", "keep only"]);
    write_file(&repo.join("skip.txt"), "b2\n");
    gs_ok(&repo, &["add", "skip.txt"]);
    gs_ok(&repo, &["commit", "skip only"]);

    let v = gs_json(&repo, &["log", "--", "keep.txt"]);
    let subjects: Vec<_> = v["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, vec!["keep only", "both"]);
    assert_eq!(v["next"], Value::Null);
    assert!(v["commits"][0]["author"]["name"].as_str().is_some());
    Ok(())
}

#[test]
fn log_json_path_history_returns_all_matches_beyond_default_page() -> TestResult {
    let scratch = Scratch::new("log-path-page")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("tracked.txt"), "seed\n");
    write_file(&repo.join("noise.txt"), "n\n");
    gs_ok(&repo, &["add", "."]);
    gs_ok(&repo, &["commit", "seed"]);
    for i in 0..12 {
        write_file(&repo.join("tracked.txt"), &format!("v{i}\n"));
        gs_ok(&repo, &["add", "tracked.txt"]);
        gs_ok(&repo, &["commit", &format!("touch tracked {i}")]);
        write_file(&repo.join("noise.txt"), &format!("n{i}\n"));
        gs_ok(&repo, &["add", "noise.txt"]);
        gs_ok(&repo, &["commit", &format!("noise only {i}")]);
    }
    let v = gs_json(&repo, &["log", "--", "tracked.txt"]);
    let commits = v["commits"].as_array().unwrap();
    assert_eq!(
        commits.len(),
        13,
        "expected seed plus 12 touches on tracked.txt, got {}",
        commits.len()
    );
    assert_eq!(commits[0]["subject"], "touch tracked 11");
    assert_eq!(commits[12]["subject"], "seed");
    assert_eq!(v["next"], Value::Null);

    let capped = gs_json(&repo, &["log", "--limit", "5", "--", "tracked.txt"]);
    assert_eq!(capped["commits"].as_array().unwrap().len(), 5);
    Ok(())
}

#[test]
fn log_rejects_before_with_paths() -> TestResult {
    let scratch = Scratch::new("log-before-path")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "x\n");
    gs_ok(&repo, &["commit", "one"]);
    let v = gs_json(&repo, &["log"]);
    let next = v["next"].as_str().unwrap_or("lw1.fake");
    let out = gs(&repo, &["log", &format!("--before={next}"), "--", "a.txt"]);
    assert_ne!(out.status, Some(0));
    assert!(
        out.stderr.contains("path filters"),
        "stderr: {}",
        out.stderr
    );
    Ok(())
}

#[test]
fn merge_conflict_json_is_structured() -> TestResult {
    let scratch = Scratch::new("merge-conflict")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("f"), "a\n");
    gs_ok(&repo, &["commit", "base"]);
    gs_ok(&repo, &["switch", "-c", "side"]);
    write_file(&repo.join("f"), "s\n");
    gs_ok(&repo, &["commit", "side"]);
    gs_ok(&repo, &["switch", "main"]);
    write_file(&repo.join("f"), "m\n");
    gs_ok(&repo, &["commit", "main"]);

    let out = gs(&repo, ["--json", "merge", "side"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(v["result"], "conflicts");
    assert_eq!(v["conflicts"], serde_json::json!(["f"]));
    assert!(
        repo.join(".git/MERGE_HEAD").exists(),
        "merge should leave MERGE_HEAD"
    );
    Ok(())
}

#[test]
fn bad_filter_and_missing_revision_errors_are_clean() -> TestResult {
    let scratch = Scratch::new("clean-errors")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    let out = gs(&repo, ["--json", "--filter", ".commits[", "log"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout)?;
    let err = v["error"].as_str().unwrap();
    assert!(!err.contains("File {"), "leaked Debug: {err}");
    assert!(err.contains("syntax error"), "{err}");

    let out = gs(&repo, ["show", "nonexistent", "--json"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(v["error"], "revision 'nonexistent' not found");

    let out = gs(&repo, ["switch", "nonexistent", "--json"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(v["error"], "no branch named 'nonexistent'");
    Ok(())
}

#[test]
fn clap_usage_error_emits_json_on_stdout() -> TestResult {
    let scratch = Scratch::new("clap-json")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    let out = gs(&repo, ["--json", "switch"]);
    assert_eq!(out.status, Some(2), "{}", out.dump());
    assert!(out.stderr.is_empty(), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout).expect("JSON on stdout");
    let err = v["error"].as_str().expect("error string");
    assert!(
        err.contains("<NAME>"),
        "JSON error must name the missing argument: {err}"
    );
    assert!(
        err.contains("required arguments were not provided"),
        "JSON error must retain clap's required-argument context: {err}"
    );
    Ok(())
}

#[test]
fn merge_json_fast_forward_and_up_to_date() -> TestResult {
    let scratch = Scratch::new("merge")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "1\n");
    gs_ok(&repo, &["commit", "base"]);

    gs_ok(&repo, &["switch", "-c", "topic"]);
    write_file(&repo.join("b.txt"), "2\n");
    gs_ok(&repo, &["commit", "topic work"]);
    gs_ok(&repo, &["switch", "main"]);

    let merged = gs_json(&repo, &["merge", "topic"]);
    assert_eq!(merged["result"], "fast_forward");
    assert_eq!(merged["branch"], "topic");
    assert!(merged["oid"].as_str().is_some());

    let again = gs_json(&repo, &["merge", "topic"]);
    assert_eq!(again["result"], "up_to_date");
    Ok(())
}

// ---------------------------------------------------------------------------
// Contract: errors, flag placement, schema stability
// ---------------------------------------------------------------------------

#[test]
fn error_contract_is_a_json_object_on_stdout() -> TestResult {
    let scratch = Scratch::new("error")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    // Empty commit message → failure.
    let out = gs(&repo, ["--json", "commit"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    assert!(
        out.stderr.is_empty(),
        "stderr should be empty: {}",
        out.dump()
    );
    let v: Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("error stdout not JSON ({e}): {}", out.dump()));
    assert!(v["error"].is_string(), "expected an error string: {v}");
    Ok(())
}

#[test]
fn json_flag_works_before_or_after_subcommand() -> TestResult {
    let scratch = Scratch::new("flag")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);

    let before = gs(&repo, ["--json", "status"]);
    let after = gs(&repo, ["status", "--json"]);
    assert_eq!(before.status, Some(0));
    assert_eq!(after.status, Some(0));
    let a: Value = serde_json::from_str(&before.stdout).unwrap();
    let b: Value = serde_json::from_str(&after.stdout).unwrap();
    assert_eq!(a, b, "flag placement must not change output");
    Ok(())
}

/// Locks the exact top-level key set of each major command's JSON. Adding,
/// renaming, or removing a field intentionally breaks this test — the guardrail
/// for a stable schema.
#[test]
fn json_filter_selects_fields() -> TestResult {
    let scratch = Scratch::new("filter")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    write_file(&repo.join("a.txt"), "hi\n");
    gs_ok(&repo, &["commit", "first"]);

    let out = gs(&repo, ["--json", "--filter", ".branch", "status"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v, Value::String("main".to_owned()));

    let out = gs(&repo, ["status", "--json", "--filter", "{branch, clean}"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["branch"], "main");
    assert_eq!(v["clean"], Value::Bool(true));
    assert!(!v.as_object().unwrap().contains_key("staged"));

    let out = gs(&repo, ["--filter", ".branch", "status"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    assert!(
        out.stderr.contains("--filter requires --json"),
        "{}",
        out.dump()
    );
    Ok(())
}

#[test]
fn json_filter_maps_commit_subjects() -> TestResult {
    let scratch = Scratch::new("filter-log")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "."]);
    for i in 0..3 {
        write_file(&repo.join("a.txt"), &format!("v{i}\n"));
        gs_ok(&repo, &["commit", &format!("commit {i}")]);
    }

    let out = gs(&repo, ["--json", "--filter", ".commits[].subject", "log"]);
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let subjects: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["commit 2", "commit 1", "commit 0"]);
    Ok(())
}

#[test]
fn schema_top_level_keys_are_stable() -> TestResult {
    let scratch = Scratch::new("schema")?;
    let seed = scratch.child("seed");
    let remote = scratch.child("remote.git");
    fs::create_dir_all(&seed)?;
    gs_ok(&seed, &["init", "."]);
    write_file(&seed.join("a.txt"), "hi\n");

    let added = gs_json(&seed, &["add"]);
    assert_eq!(keys(&added), ["staged"]);

    write_file(&seed.join("drop.txt"), "bye\n");
    gs_ok(&seed, &["add", "drop.txt"]);
    gs_ok(&seed, &["commit", "track drop"]);
    let removed = gs_json(&seed, &["rm", "drop.txt"]);
    assert_eq!(keys(&removed), ["removed"]);
    assert_eq!(removed["removed"][0], "drop.txt");

    write_file(&seed.join("from.txt"), "mv\n");
    gs_ok(&seed, &["add", "from.txt"]);
    gs_ok(&seed, &["commit", "track from"]);
    let renamed = gs_json(&seed, &["mv", "from.txt", "to.txt"]);
    assert_eq!(keys(&renamed), ["from", "to"]);
    assert_eq!(renamed["from"], "from.txt");
    assert_eq!(renamed["to"], "to.txt");

    write_file(&seed.join("scratch.txt"), "x\n");
    let clean_preview = gs_json(&seed, &["clean"]);
    assert_eq!(keys(&clean_preview), ["dry_run", "removed"]);
    assert_eq!(clean_preview["dry_run"], Value::Bool(true));
    assert!(clean_preview["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p.as_str() == Some("scratch.txt")));
    assert!(seed.join("scratch.txt").is_file());
    let clean_forced = gs_json(&seed, &["clean", "-f"]);
    assert_eq!(keys(&clean_forced), ["dry_run", "removed"]);
    assert_eq!(clean_forced["dry_run"], Value::Bool(false));
    assert!(!seed.join("scratch.txt").is_file());
    let committed = gs_json(&seed, &["commit", "first"]);
    assert_eq!(
        keys(&committed),
        ["amended", "branch", "changes", "oid", "subject"]
    );

    let status = gs_json(&seed, &["status"]);
    assert_eq!(
        keys(&status),
        [
            "ahead",
            "behind",
            "branch",
            "clean",
            "commits",
            "detached",
            "head",
            "merging",
            "staged",
            "target",
            "unstaged",
            "untracked"
        ]
    );

    let log = gs_json(&seed, &["log"]);
    assert_eq!(keys(&log), ["commits", "next"]);

    write_file(&seed.join("blame.txt"), "hello\n");
    gs_ok(&seed, &["add", "blame.txt"]);
    gs_ok(&seed, &["commit", "add blame file"]);
    let blame = gs_json(&seed, &["blame", "blame.txt"]);
    assert_eq!(keys(&blame), ["commits", "file", "lines", "rev"]);
    assert!(blame["lines"].as_array().is_some_and(|a| !a.is_empty()));

    let branches = gs_json(&seed, &["branch"]);
    assert_eq!(keys(&branches), ["action", "branches", "current"]);

    // push / fetch over a local remote.
    gs_ok(scratch.path(), &["init", "--bare", &path_arg(&remote)]);
    gs_ok(&seed, &["remote", "add", "origin", &path_arg(&remote)]);
    let push = gs_json(&seed, &["push"]);
    assert_eq!(keys(&push), ["branch", "rejected", "remote", "results"]);
    assert_eq!(
        keys(&push["results"][0]),
        ["ref", "status"],
        "an `ok` push result omits the optional `reason`"
    );

    let fetch = gs_json(&seed, &["fetch"]);
    assert_eq!(keys(&fetch), ["remote", "updated", "updates"]);

    write_file(&seed.join("a.txt"), "dirty\n");
    let restored = gs_json(&seed, &["restore", "a.txt"]);
    assert_eq!(keys(&restored), ["removed", "restored"]);

    Ok(())
}
