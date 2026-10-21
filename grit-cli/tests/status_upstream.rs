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
fn status_ahead_behind_config_false() -> TestResult {
    let scratch = Scratch::new("config")?;
    let clone = setup_topic_vs_main(&scratch);
    gs_ok(&clone, &["config", "status.aheadBehind", "false"]);
    let v = gs_json(&clone, &["status"]);
    assert_eq!(v["ahead"], Value::Null);
    assert_eq!(v["behind"], Value::Null);
    Ok(())
}
