//! Status reports target and upstream separately (`#2804`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "grit-cli-status-upstream-{tag}-{}-{n}",
            std::process::id()
        ));
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

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn gs(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GS)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit")
}

fn gs_ok(dir: &Path, args: &[&str]) {
    let out = gs(dir, args);
    assert!(
        out.status.success(),
        "grit {} failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn gs_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = gs(dir, &full);
    assert!(out.status.success(), "grit status json failed");
    serde_json::from_slice(&out.stdout).expect("json")
}

fn system_git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn path_arg(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

fn write_file(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write");
}

fn setup_topic_vs_main(scratch: &Scratch) -> PathBuf {
    let remote = scratch.child("r.git");
    let upstream = scratch.child("w");
    let clone = scratch.child("c");
    fs::create_dir_all(&upstream).expect("mkdir");
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
    gs_ok(&clone, &["config", "target.branch", "main"]);
    clone
}

#[test]
fn status_reports_target_and_upstream() -> TestResult {
    let scratch = Scratch::new("both")?;
    let clone = setup_topic_vs_main(&scratch);
    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["branch"], "topic");
    assert_eq!(v["target"], "main");
    assert_eq!(v["upstream"]["ref"], "origin/topic");
    assert_eq!(v["upstream"]["ahead"], 0);
    assert_eq!(v["upstream"]["behind"], 0);
    Ok(())
}

#[test]
fn status_no_ahead_behind_null_counts() -> TestResult {
    let scratch = Scratch::new("no-counts")?;
    let clone = setup_topic_vs_main(&scratch);
    let v = gs_json(&clone, &["status", "--no-ahead-behind"]);
    assert_eq!(v["ahead"], Value::Null);
    assert_eq!(v["behind"], Value::Null);
    assert_eq!(v["upstream"]["ahead"], Value::Null);
    assert_eq!(v["upstream"]["behind"], Value::Null);
    Ok(())
}

#[test]
fn status_upstream_without_target_branch() -> TestResult {
    let scratch = Scratch::new("upstream-only")?;
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
    system_git_ok(&upstream, &["commit", "-qm", "topic"]);
    system_git_ok(&upstream, &["push", "-u", "origin", "HEAD:topic"]);
    system_git_ok(
        scratch.path(),
        &[
            "clone",
            "-q",
            "-b",
            "topic",
            &path_arg(&remote),
            &path_arg(&clone),
        ],
    );

    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["branch"], "topic");
    assert!(v["target"].is_null());
    assert_eq!(v["upstream"]["ref"], "origin/topic");
    assert_eq!(v["upstream"]["ahead"], 0);
    assert_eq!(v["upstream"]["behind"], 0);

    let human = gs(&clone, &["status"]);
    assert!(
        human.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&human.stderr)
    );
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("even with origin/topic"),
        "expected upstream in header:\n{stdout}"
    );
    Ok(())
}

#[test]
fn status_pull_hint_follows_upstream_not_target() -> TestResult {
    let scratch = Scratch::new("pull-hint")?;
    let clone = setup_topic_vs_main(&scratch);
    let upstream = scratch.child("w");
    system_git_ok(&upstream, &["checkout", "-q", "main"]);
    write_file(&upstream.join("on-main.txt"), "main-only\n");
    system_git_ok(&upstream, &["add", "on-main.txt"]);
    system_git_ok(&upstream, &["commit", "-qm", "advance main"]);
    system_git_ok(&upstream, &["push", "-q", "origin", "HEAD:main"]);

    system_git_ok(&clone, &["fetch", "-q"]);
    system_git_ok(&clone, &["checkout", "-q", "main"]);
    system_git_ok(&clone, &["merge", "-q", "--ff-only", "origin/main"]);
    system_git_ok(&clone, &["checkout", "-q", "topic"]);

    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["branch"], "topic");
    assert_eq!(v["target"], "main");
    assert_eq!(v["upstream"]["ref"], "origin/topic");
    assert_eq!(v["upstream"]["ahead"], 0);
    assert_eq!(v["upstream"]["behind"], 0);
    assert_eq!(v["behind"], 1);

    let human = gs(&clone, &["status"]);
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        !stdout.contains("grit pull to update from main"),
        "pull hint must not use target when upstream is distinct and even:\n{stdout}"
    );
    assert!(
        !stdout.contains("grit pull to update from origin/main"),
        "pull hint must not use target remote when upstream is even:\n{stdout}"
    );

    write_file(&upstream.join("on-topic.txt"), "remote topic\n");
    system_git_ok(&upstream, &["checkout", "-q", "topic"]);
    system_git_ok(&upstream, &["add", "on-topic.txt"]);
    system_git_ok(&upstream, &["commit", "-qm", "advance topic"]);
    system_git_ok(&upstream, &["push", "-q", "origin", "HEAD:topic"]);
    system_git_ok(&clone, &["fetch", "-q"]);

    let human = gs(&clone, &["status"]);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("grit pull to update from origin/topic"),
        "pull hint should track upstream behind counts:\n{stdout}"
    );
    Ok(())
}

#[test]
fn status_markdown_includes_upstream_line() -> TestResult {
    let scratch = Scratch::new("md-upstream")?;
    let clone = setup_topic_vs_main(&scratch);
    let out = Command::new(GS)
        .args(["--markdown", "status"])
        .current_dir(&clone)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("origin/topic"),
        "markdown should name upstream ref:\n{stdout}"
    );
    let header_end = stdout
        .find("Staged")
        .or_else(|| stdout.find("Nothing to commit"))
        .unwrap_or(stdout.len());
    let header = &stdout[..header_end];
    assert!(
        header.contains('↑') && header.contains('↓'),
        "upstream counts should appear in header before body:\n{header}"
    );
    Ok(())
}

#[test]
fn status_human_shows_upstream_counts_under_header_before_shortlog() -> TestResult {
    let scratch = Scratch::new("upstream-order")?;
    let clone = setup_topic_vs_main(&scratch);
    let human = gs(&clone, &["status"]);
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    let up_pos = stdout.find("↑").expect("upstream counts line");
    let shortlog_pos = stdout
        .find("change ")
        .or_else(|| stdout.find("commit "))
        .unwrap_or(usize::MAX);
    assert!(
        up_pos < shortlog_pos,
        "upstream line must precede ahead shortlog:\n{stdout}"
    );
    assert!(stdout.contains("origin/topic"));
    Ok(())
}

#[test]
fn status_pull_hint_prefers_upstream_over_target_when_both_exist() -> TestResult {
    let scratch = Scratch::new("pull-upstream-pref")?;
    let clone = setup_topic_vs_main(&scratch);
    let upstream = scratch.child("w");
    system_git_ok(&upstream, &["checkout", "-q", "topic"]);
    write_file(&upstream.join("remote.txt"), "x\n");
    system_git_ok(&upstream, &["add", "remote.txt"]);
    system_git_ok(&upstream, &["commit", "-qm", "remote"]);
    system_git_ok(&upstream, &["push", "-q", "origin", "HEAD:topic"]);
    system_git_ok(&clone, &["fetch", "-q"]);

    let human = gs(&clone, &["status"]);
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("grit pull to update from origin/topic"),
        "pull hint must use upstream, not target main:\n{stdout}"
    );
    assert!(!stdout.contains("grit pull to update from main"));
    Ok(())
}

#[test]
fn status_markdown_upstream_line_without_target() -> TestResult {
    let scratch = Scratch::new("md-up-only")?;
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
    system_git_ok(&upstream, &["commit", "-qm", "topic"]);
    system_git_ok(&upstream, &["push", "-u", "origin", "HEAD:topic"]);
    system_git_ok(
        scratch.path(),
        &[
            "clone",
            "-q",
            "-b",
            "topic",
            &path_arg(&remote),
            &path_arg(&clone),
        ],
    );

    let out = Command::new(GS)
        .args(["--markdown", "status"])
        .current_dir(&clone)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("origin/topic"),
        "markdown should name upstream when no target:\n{stdout}"
    );
    Ok(())
}

#[test]
fn status_merge_hint_for_local_branch_behind_target() -> TestResult {
    let scratch = Scratch::new("issue-1990")?;
    let repo = scratch.child("r");
    fs::create_dir_all(&repo)?;
    gs_ok(&repo, &["init", "-b", "main"]);
    write_file(&repo.join("a"), "1\n");
    gs_ok(&repo, &["add", "a"]);
    gs_ok(&repo, &["commit", "-m", "c1"]);
    gs_ok(&repo, &["switch", "-c", "old"]);
    gs_ok(&repo, &["switch", "main"]);
    write_file(&repo.join("b"), "2\n");
    gs_ok(&repo, &["add", "b"]);
    gs_ok(&repo, &["commit", "-m", "c2"]);
    gs_ok(&repo, &["switch", "old"]);

    let human = gs(&repo, &["status"]);
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("grit merge main"),
        "expected merge hint for local-only repo:\n{stdout}"
    );
    assert!(
        !stdout.contains("grit pull to update from main"),
        "pull hint must not appear without upstream:\n{stdout}"
    );

    let merge = gs(&repo, &["merge", "main"]);
    assert!(merge.status.success());
    let merge_out = String::from_utf8_lossy(&merge.stdout);
    assert!(
        merge_out.contains("Fast-forwarded old →") && merge_out.contains("(main)"),
        "merge should name the branch that moved:\n{merge_out}"
    );
    Ok(())
}

#[test]
fn status_ahead_behind_config_false() -> TestResult {
    let scratch = Scratch::new("config")?;
    let clone = setup_topic_vs_main(&scratch);
    gs_ok(&clone, &["config", "status.aheadBehind", "false"]);
    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["ahead"], Value::Null);
    assert_eq!(v["behind"], Value::Null);
    Ok(())
}
