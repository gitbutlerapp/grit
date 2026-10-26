//! Git-habit argv (`checkout -b`, `reset HEAD`, `add -A`, …) maps to grit spellings.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
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

struct Failure {
    stderr: String,
    json: Value,
}

fn fails(dir: &Path, home: &Path, args: &[&str]) -> Failure {
    fails_with(dir, home, args, 2)
}

fn fails_with(dir: &Path, home: &Path, args: &[&str], code: i32) -> Failure {
    let human = grit(dir, home, args);
    assert_eq!(human.status.code(), Some(code), "grit {args:?}");
    let mut json_args = vec!["--json"];
    json_args.extend_from_slice(args);
    let json = grit(dir, home, &json_args);
    assert_eq!(json.status.code(), Some(code), "grit {json_args:?}");
    Failure {
        stderr: String::from_utf8_lossy(&human.stderr).into_owned(),
        json: serde_json::from_slice(&json.stdout).expect("JSON error on stdout"),
    }
}

impl Failure {
    fn assert_hint(&self, kind: &str, hint_fragment: &str) {
        assert_eq!(self.json["kind"], kind, "{}", self.json);
        let hint = self.json["hint"].as_str().expect("hint");
        assert!(hint.contains(hint_fragment), "{hint}");
        assert!(
            self.stderr.contains(&format!("\nhint: {hint}")),
            "stderr lacks hint line: {}",
            self.stderr
        );
    }

    fn assert_usage_hint(&self, hint_fragment: &str) {
        self.assert_hint("usage", hint_fragment);
        assert!(
            !self.stderr.contains("tip: to pass"),
            "misleading clap tip still present: {}",
            self.stderr
        );
    }
}

fn fresh_repo() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = tempfile::tempdir().expect("repo");
    let home = tempfile::tempdir().expect("home");
    ok(repo.path(), home.path(), &["init", "."]);
    fs::write(repo.path().join("a"), "a\n").expect("write");
    ok(repo.path(), home.path(), &["commit", "-m", "one"]);
    (repo, home)
}

#[test]
fn checkout_dash_b_suggests_switch_create() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["checkout", "-b", "feature"]);
    failure.assert_usage_hint("grit checkout -c feature");
    assert!(
        failure.stderr.contains("Usage: grit checkout"),
        "{}",
        failure.stderr
    );
}

#[test]
fn checkout_path_suggests_restore_not_new_branch() {
    let (repo, home) = fresh_repo();
    fs::write(repo.path().join("a"), "2\n").expect("write");
    for args in [&["checkout", "--", "a"][..], &["checkout", "a"][..]] {
        let failure = fails_with(repo.path(), home.path(), args, 1);
        failure.assert_hint("branch_not_found", "grit restore a");
        assert!(
            !failure.json["hint"]
                .as_str()
                .unwrap_or("")
                .contains("switch -c a"),
            "{}",
            failure.json
        );
    }
}

#[test]
fn reset_head_path_suggests_restore_staged() {
    let (repo, home) = fresh_repo();
    fs::write(repo.path().join("n"), "n\n").expect("write n");
    ok(repo.path(), home.path(), &["add", "n"]);
    let failure = fails(repo.path(), home.path(), &["reset", "HEAD", "n"]);
    failure.assert_usage_hint("grit restore --staged n");
}

#[test]
fn add_dash_a_suggests_bare_add() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["add", "-A"]);
    failure.assert_usage_hint("no paths stages everything");
}

#[test]
fn init_dash_q_notes_no_quiet_flag() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["init", "-q", "other"]);
    failure.assert_usage_hint("no quiet flag");
}

#[test]
fn markdown_usage_hint_for_checkout_dash_b() {
    let (repo, home) = fresh_repo();
    let out = grit(
        repo.path(),
        home.path(),
        &["checkout", "-b", "feature", "--markdown"],
    );
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("**Hint:**"));
    assert!(stdout.contains("grit checkout -c feature"));
}
