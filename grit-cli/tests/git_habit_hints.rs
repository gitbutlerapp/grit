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
    failure.assert_usage_hint("grit switch -c feature");
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
    failure.assert_usage_hint("unstage paths with `grit restore --staged n`");
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
    assert!(stdout.contains("grit switch -c feature"));
}

#[test]
fn status_dash_s_succeeds() {
    let (repo, home) = fresh_repo();
    ok(repo.path(), home.path(), &["status", "-s"]);
}

#[test]
fn log_dash_one_succeeds() {
    let (repo, home) = fresh_repo();
    ok(repo.path(), home.path(), &["log", "-1"]);
    ok(
        repo.path(),
        home.path(),
        &["log", "--graph", "--oneline", "-3"],
    );
}

#[test]
fn tag_dash_a_with_message_succeeds() {
    let (repo, home) = fresh_repo();
    ok(repo.path(), home.path(), &["tag", "-a", "v3", "-m", "x"]);
}

#[test]
fn tag_annotate_without_message_is_usage() {
    let (repo, home) = fresh_repo();
    for args in [
        &["tag", "-a", "review-no-message", "--dry-run"][..],
        &["tag", "-a"][..],
    ] {
        let failure = fails(repo.path(), home.path(), args);
        assert!(
            failure.stderr.contains("annotated tag requires a message"),
            "{}",
            failure.stderr
        );
        assert_eq!(failure.json["kind"], "usage", "{}", failure.json);
    }
    let json_only = grit(
        repo.path(),
        home.path(),
        &["--json", "tag", "-a", "review-no-message", "--dry-run"],
    );
    assert_eq!(json_only.status.code(), Some(2));
    let body: serde_json::Value = serde_json::from_slice(&json_only.stdout).expect("usage json");
    assert_eq!(body["kind"], "usage");
}

#[test]
fn restore_dash_s_source_succeeds() {
    let (repo, home) = fresh_repo();
    fs::write(repo.path().join("a"), "2\n").expect("write");
    ok(repo.path(), home.path(), &["commit", "-m", "two"]);
    fs::write(repo.path().join("a"), "3\n").expect("write");
    ok(repo.path(), home.path(), &["restore", "-s", "HEAD~1", "a"]);
}

#[test]
fn commit_repeated_m_joins_paragraphs() {
    let (repo, home) = fresh_repo();
    fs::write(repo.path().join("b"), "b\n").expect("write");
    ok(
        repo.path(),
        home.path(),
        &["commit", "-m", "title", "-m", "body"],
    );
    let log = grit(repo.path(), home.path(), &["log", "-n", "1", "--json"]);
    assert!(log.status.success());
    let json: serde_json::Value = serde_json::from_slice(&log.stdout).expect("json");
    assert_eq!(json["commits"][0]["message"], "title\n\nbody");
}

#[test]
fn commit_amend_no_edit_hint() {
    let (repo, home) = fresh_repo();
    let failure = fails(
        repo.path(),
        home.path(),
        &["commit", "--amend", "--no-edit"],
    );
    failure.assert_usage_hint("does not open an editor");
    assert!(!failure.stderr.contains("as text"));
}

#[test]
fn clean_fd_hint() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["clean", "-fd"]);
    failure.assert_usage_hint("grit clean -f");
    assert!(!failure.stderr.contains("tip: to pass"));
}

#[test]
fn mutating_dash_n_suggests_dry_run() {
    let (repo, home) = fresh_repo();
    fs::write(repo.path().join("u"), "u\n").expect("write u");
    let fragment = "use --dry-run (`-n` is a list limit in grit)";
    for args in [
        &["clean", "-n"][..],
        &["add", "-n", "u"][..],
        &["rm", "-n", "a"][..],
        &["mv", "-n", "a", "b"][..],
        &["push", "-n"][..],
    ] {
        let failure = fails(repo.path(), home.path(), args);
        failure.assert_usage_hint(fragment);
    }
    let out = grit(repo.path(), home.path(), &["--json", "clean", "-n"]);
    assert_eq!(out.status.code(), Some(2));
    let body: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(body["kind"], "usage");
    assert!(body["hint"].as_str().unwrap().contains(fragment));
}

#[test]
fn stash_save_hint() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["stash", "save", "wip"]);
    failure.assert_usage_hint("stash push -m wip");
}

#[test]
fn cherry_pick_points_at_pick() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["cherry-pick", "HEAD"]);
    failure.assert_usage_hint("grit pick");
    assert!(!failure.stderr.contains("receive-pack"));
}

#[test]
fn rev_parse_points_at_resolve() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["rev-parse", "HEAD"]);
    failure.assert_usage_hint("grit resolve");
}

#[test]
fn cat_file_points_at_cat_and_resolve() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["cat-file", "-p", "HEAD"]);
    failure.assert_usage_hint("grit cat");
    assert!(failure.json["hint"]
        .as_str()
        .unwrap()
        .contains("grit resolve"));
}

#[test]
fn ls_files_points_at_status_json() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["ls-files"]);
    failure.assert_usage_hint("grit status --json");
}

#[test]
fn describe_points_at_log_and_tag_contains() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["describe"]);
    failure.assert_usage_hint("grit log -n 1 --decorate");
    assert!(failure.json["hint"]
        .as_str()
        .unwrap()
        .contains("tag --contains"));
}

#[test]
fn grep_points_at_log_grep_not_repack() {
    let (repo, home) = fresh_repo();
    let failure = fails(repo.path(), home.path(), &["grep", "foo"]);
    failure.assert_usage_hint("grit log --grep");
    assert!(!failure.stderr.contains("repack"));
}

#[test]
fn checkout_slash_or_dotted_branch_names_suggest_switch_create() {
    let (repo, home) = fresh_repo();
    for branch in ["feature/foo", "release-1.0"] {
        let failure = fails_with(repo.path(), home.path(), &["checkout", branch], 1);
        failure.assert_hint("branch_not_found", &format!("grit switch -c {branch}"));
        let hint = failure.json["hint"].as_str().unwrap_or("");
        assert!(
            !hint.contains("restore"),
            "branch name {branch} must not look like restore: {hint}"
        );
    }
}
