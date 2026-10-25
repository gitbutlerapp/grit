//! First-push, pull and merge failures say what happened and which command to run next.
//!
//! Each case checks the human `error:` / `hint:` lines on stderr and the `--json` object's
//! `kind`, `hint` and (where relevant) `paths`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .expect("run grit")
}

fn ok(dir: &Path, home: &Path, args: &[&str]) {
    let out = grit(dir, home, args);
    assert!(
        out.status.success(),
        "grit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Human stderr plus the parsed `--json` error for the same command.
struct Failure {
    stderr: String,
    json: Value,
}

fn fails(dir: &Path, home: &Path, args: &[&str]) -> Failure {
    fails_with(dir, home, args, 1)
}

fn fails_with(dir: &Path, home: &Path, args: &[&str], code: i32) -> Failure {
    let human = grit(dir, home, args);
    assert_eq!(human.status.code(), Some(code), "grit {args:?}");
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let json = grit(dir, home, &json_args);
    assert_eq!(json.status.code(), Some(code), "grit {json_args:?}");
    Failure {
        stderr: String::from_utf8_lossy(&human.stderr).into_owned(),
        json: serde_json::from_slice(&json.stdout).expect("JSON error on stdout"),
    }
}

impl Failure {
    fn assert_kind_and_hint(&self, kind: &str, hint_fragment: &str) {
        assert_eq!(self.json["kind"], kind, "{}", self.json);
        let hint = self.json["hint"].as_str().expect("hint");
        assert!(hint.contains(hint_fragment), "{hint}");
        assert!(
            self.stderr.contains(&format!("\nhint: {hint}")),
            "stderr lacks the hint line: {}",
            self.stderr
        );
    }
}

/// A repository with one commit and an empty `HOME` (no credential helper).
fn fresh_repo() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().expect("repo");
    let home = tempfile::tempdir().expect("home");
    ok(repo.path(), home.path(), &["init", "."]);
    fs::write(repo.path().join("a"), "a\n").expect("write");
    ok(repo.path(), home.path(), &["commit", "-m", "one"]);
    (repo, home)
}

/// Serve every request with `status` until the test process exits.
fn serve_status(status: &'static str, extra_headers: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                line.clear();
            }
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\n{extra_headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    format!("http://{addr}/team/repo.git")
}

#[test]
fn push_without_any_remote_suggests_remote_add() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["push"]);
    assert_eq!(failure.json["error"], "no remote to push to");
    failure.assert_kind_and_hint("no_remote", "grit remote add origin <url>");

    let failure = fails(repo.path(), home.path(), &["push", "origin"]);
    assert_eq!(failure.json["kind"], "no_remote");
}

#[test]
fn push_and_pull_to_an_unknown_remote_suggest_remote_add() {
    let (repo, home) = fresh_repo();
    for args in [&["push", "origin", "main"][..], &["pull"][..]] {
        let failure = fails(repo.path(), home.path(), args);
        assert_eq!(failure.json["error"], "unknown remote 'origin'", "{args:?}");
        failure.assert_kind_and_hint("remote_not_found", "grit remote add origin <url>");
    }
}

#[test]
fn url_less_remote_suggests_set_url() {
    let (repo, home) = fresh_repo();
    ok(
        repo.path(),
        home.path(),
        &[
            "config",
            "remote.half.fetch",
            "+refs/heads/*:refs/remotes/half/*",
        ],
    );
    let failure = fails(repo.path(), home.path(), &["fetch", "half"]);
    assert_eq!(failure.json["error"], "remote 'half' has no URL configured");
    failure.assert_kind_and_hint("remote_no_url", "grit remote set-url half <url>");
}

#[test]
fn push_without_credentials_names_the_repository_and_a_credential_helper() {
    let (repo, home) = fresh_repo();
    let url = serve_status(
        "401 Unauthorized",
        "WWW-Authenticate: Basic realm=\"x\"\r\n",
    );
    ok(repo.path(), home.path(), &["remote", "add", "origin", &url]);
    let failure = fails(repo.path(), home.path(), &["push"]);
    let error = failure.json["error"].as_str().expect("error");
    assert!(
        error.starts_with(&format!("authentication failed for {url}: ")),
        "{error}"
    );
    assert!(
        !error.contains("info/refs") && !error.contains("service="),
        "{error}"
    );
    failure.assert_kind_and_hint("auth_required", "credential.helper");
}

#[test]
fn missing_repository_is_reported_without_protocol_details() {
    let (repo, home) = fresh_repo();
    let url = serve_status("404 Not Found", "");
    let failure = fails_with(repo.path(), home.path(), &["clone", &url, "copy"], 128);
    assert_eq!(failure.json["error"], format!("HTTP 404 from {url}"));
    failure.assert_kind_and_hint("remote_unreadable", "check the URL");
}

#[test]
fn pull_without_upstream_suggests_pull_by_name_or_push_u() {
    let (repo, home) = fresh_repo();
    let upstream = tempfile::tempdir().expect("upstream");
    let status = Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(upstream.path())
        .status()
        .expect("git init");
    assert!(status.success());
    ok(
        repo.path(),
        home.path(),
        &["remote", "add", "origin", upstream.path().to_str().unwrap()],
    );
    let failure = fails(repo.path(), home.path(), &["pull"]);
    assert_eq!(
        failure.json["error"],
        "no upstream configured for branch main"
    );
    failure.assert_kind_and_hint("branch_no_upstream", "grit push -u origin main");
    assert!(failure.stderr.contains("grit pull origin main"));
}

#[test]
fn merge_continue_with_markers_lists_paths_and_next_steps() {
    let (repo, home) = fresh_repo();
    let dir = repo.path();
    let home = home.path();
    fs::write(dir.join("f.txt"), "l1\nl2\nl3\n").expect("write");
    ok(dir, home, &["commit", "-m", "base"]);
    ok(dir, home, &["switch", "-c", "other"]);
    fs::write(dir.join("f.txt"), "l1\nO\nl3\n").expect("write");
    ok(dir, home, &["commit", "-m", "other"]);
    ok(dir, home, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "l1\nM\nl3\n").expect("write");
    ok(dir, home, &["commit", "-m", "main"]);
    let merge = grit(dir, home, &["merge", "other"]);
    assert_eq!(merge.status.code(), Some(1));

    let failure = fails(dir, home, &["merge", "--continue"]);
    assert_eq!(
        failure.json["error"],
        "you still have unmerged paths: f.txt"
    );
    assert_eq!(failure.json["paths"], serde_json::json!(["f.txt"]));
    failure.assert_kind_and_hint("unmerged_paths", "grit merge --continue");
    assert!(!failure.stderr.contains("[\""), "{}", failure.stderr);
}

#[test]
fn switch_over_local_changes_names_files_and_suggests_stash() {
    let (repo, home) = fresh_repo();
    let dir = repo.path();
    let home = home.path();
    ok(dir, home, &["switch", "-c", "other"]);
    fs::write(dir.join("a"), "other\n").expect("write");
    ok(dir, home, &["commit", "-m", "other"]);
    ok(dir, home, &["switch", "main"]);
    fs::write(dir.join("a"), "dirty\n").expect("write");

    let failure = fails(dir, home, &["switch", "other"]);
    assert_eq!(
        failure.json["would_overwrite"]["paths"],
        serde_json::json!(["a"])
    );
    failure.assert_kind_and_hint("would_overwrite", "grit stash");
    assert!(failure.stderr.contains("\n  a\n"), "{}", failure.stderr);
}

fn empty_repo() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().expect("repo");
    let home = tempfile::tempdir().expect("home");
    ok(repo.path(), home.path(), &["init", "."]);
    (repo, home)
}

#[test]
fn fresh_repo_switch_minus_has_no_previous_branch_hint() {
    let (repo, home) = empty_repo();
    let failure = fails(repo.path(), home.path(), &["switch", "-"]);
    assert_eq!(
        failure.json["error"],
        "there is no previous branch to go back to"
    );
    assert_eq!(failure.json["kind"], "no_previous_branch");
    assert!(failure.json["hint"].is_null(), "{}", failure.json);
    assert!(
        !failure.stderr.contains("switch -c"),
        "must not suggest creating a branch: {}",
        failure.stderr
    );
}

#[test]
fn fresh_repo_log_and_show_say_no_commits_yet() {
    let (repo, home) = empty_repo();
    for args in [
        &["log"][..],
        &["show"][..],
        &["reset"][..],
        &["reset", "--hard"][..],
    ] {
        let failure = fails(repo.path(), home.path(), args);
        assert_eq!(failure.json["kind"], "branch_no_commits", "{args:?}");
        let hint = failure.json["hint"].as_str().expect("hint");
        assert!(hint.contains(r#"grit commit "message""#), "{hint}");
        assert!(
            failure.json["error"]
                .as_str()
                .expect("error")
                .contains("no commits yet"),
            "{args:?}"
        );
    }
}

#[test]
fn fresh_repo_stash_pop_with_no_entries() {
    let (repo, home) = empty_repo();
    let failure = fails(repo.path(), home.path(), &["stash", "pop"]);
    assert_eq!(failure.json["error"], "no stash entries");
    let hint = failure.json["hint"].as_str().expect("hint");
    assert!(hint.contains("grit stash push"), "{hint}");
    assert!(
        !failure.stderr.contains("stash@{"),
        "must not leak stash ref syntax: {}",
        failure.stderr
    );
}

#[test]
fn switch_to_unknown_branch_suggests_listing_or_creating() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["switch", "nonexistent"]);
    assert_eq!(failure.json["error"], "no branch named 'nonexistent'");
    failure.assert_kind_and_hint("branch_not_found", "grit switch -c nonexistent");
}

#[test]
fn markdown_errors_carry_the_hint() {
    let (repo, home) = fresh_repo();
    let out = grit(repo.path(), home.path(), &["push", "--markdown"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("**Error:** no remote to push to"),
        "{stdout}"
    );
    assert!(
        stdout.contains("**Hint:** add one with `grit remote add origin <url>`"),
        "{stdout}"
    );
}
