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
    assert_eq!(v["bare"], Value::Bool(false));
    assert_eq!(v["branch"], "main");
    assert!(
        v["path"].as_str().unwrap().ends_with(".git"),
        "path should be the .git dir: {v}"
    );
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
    let v2 = gs_json(&repo, &["log", &format!("--before={next}")]);
    assert_eq!(v2["next"], Value::Null);
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

    let switched = gs_json(&repo, &["switch", "main"]);
    assert_eq!(switched["branch"], "main");
    assert_eq!(switched["created"], Value::Bool(false));
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

    let committed = gs_json(&seed, &["commit", "first"]);
    assert_eq!(keys(&committed), ["branch", "changes", "oid", "subject"]);

    let status = gs_json(&seed, &["status"]);
    assert_eq!(
        keys(&status),
        [
            "ahead",
            "branch",
            "clean",
            "commits",
            "detached",
            "head",
            "staged",
            "target",
            "unstaged",
            "untracked"
        ]
    );

    let log = gs_json(&seed, &["log"]);
    assert_eq!(keys(&log), ["commits", "next"]);

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
    Ok(())
}
