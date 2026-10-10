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
    assert_eq!(v["commits"].as_array().unwrap().len(), 1);
    assert_eq!(v["conflicts"], serde_json::json!(["f"]));
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
    assert_eq!(del["segments"][0]["text"], "a\r");

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
    assert_eq!(log["commits"][0]["author_date"], expected);

    let show = gs_json(&repo, &["show"]);
    assert_eq!(show["commit"]["author"]["date"], expected);
    assert_eq!(show["commit"]["committer"]["date"], expected);
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
    assert_eq!(commit["author"], "test");
    assert!(commit["author_date"]
        .as_str()
        .is_some_and(|d| d.contains('T')));
    assert!(commit["relative_date"].as_str().is_some());
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
    assert_eq!(v["kind"], "conflict");
    assert_eq!(v["error"], "merge has conflicts");
    assert_eq!(v["conflicts"], serde_json::json!(["f"]));
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
    assert_eq!(keys(&clean_preview), ["removed"]);
    assert!(clean_preview["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p.as_str() == Some("scratch.txt")));
    let _ = gs_ok(&seed, &["clean", "-f"]);
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
