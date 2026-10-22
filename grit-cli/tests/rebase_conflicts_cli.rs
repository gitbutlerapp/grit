//! `grit rebase --continue` / `--abort` / `--skip` and status hints.

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

#[derive(Debug)]
struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "grit-cli-rebase-conflicts-{tag}-{}-{n}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn null_device() -> OsString {
    if cfg!(windows) {
        OsString::from("NUL")
    } else {
        OsString::from("/dev/null")
    }
}

fn gs<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new(GS)
        .args(args.into_iter().map(|a| a.as_ref().to_os_string()))
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_json(stdout: &str) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(stdout.trim())?)
}

fn init_repo(dir: &Path) -> Result<(), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("f.txt"), "1\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    Ok(())
}

/// Topic branch with one commit conflicting when replayed onto `onto`.
fn init_conflict_rebase(dir: &Path) -> Result<(String, String, String, String), Box<dyn Error>> {
    init_repo(dir)?;
    git(dir, &["checkout", "-b", "topic"])?;
    fs::write(dir.join("f.txt"), "topic\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "topic change"])?;
    let topic_tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "main"])?;
    fs::write(dir.join("f.txt"), "main\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "main change"])?;
    let onto = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let upstream = git(dir, &["rev-parse", "HEAD~1"])?.trim().to_string();
    git(dir, &["checkout", "topic"])?;
    Ok((upstream, onto, topic_tip, "f.txt".into()))
}

fn start_conflict_rebase(dir: &Path) -> Result<Value, Box<dyn Error>> {
    let (upstream, onto, _, _) = init_conflict_rebase(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(dir, ["rebase", "--onto", &onto, &upstream, "--json"])?;
    assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
    parse_json(&out.stdout)
}

#[test]
fn rebase_stops_on_conflict_json() -> TestResult {
    let scratch = Scratch::new("stop-json")?;
    let dir = scratch.path();
    let json = start_conflict_rebase(dir)?;
    assert_eq!(json["result"], "conflicts");
    assert_eq!(json["operation"], "rebase");
    assert!(json["commit"].is_string());
    assert_eq!(json["conflicts"][0]["path"], "f.txt");
    assert_eq!(json["conflicts"][0]["kind"], "content");
    let steps = json["next_steps"].as_array().expect("next_steps array");
    assert_eq!(steps[0], "grit diff");
    assert_eq!(steps[1], "grit add .");
    assert_eq!(steps[2], "grit rebase --continue");
    let alts = json["alternatives"].as_array().expect("alternatives");
    assert!(alts.iter().any(|v| v == "grit rebase --abort"));
    assert!(alts.iter().any(|v| v == "grit rebase --skip"));

    let mut snap = json.clone();
    snap["commit"] = Value::String("[commit]".into());
    insta::assert_json_snapshot!(snap, @r#"
    {
      "alternatives": [
        "grit rebase --abort",
        "grit rebase --skip"
      ],
      "commit": "[commit]",
      "conflicts": [
        {
          "kind": "content",
          "path": "f.txt"
        }
      ],
      "next_steps": [
        "grit diff",
        "grit add .",
        "grit rebase --continue"
      ],
      "operation": "rebase",
      "result": "conflicts"
    }
    "#);
    Ok(())
}

#[test]
fn rebase_continue_finishes_and_moves_refs() -> TestResult {
    let scratch = Scratch::new("continue")?;
    let dir = scratch.path();
    let (upstream, onto, topic_tip, _) = init_conflict_rebase(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    gs(dir, ["rebase", "--onto", &onto, &upstream])?;
    fs::write(dir.join("f.txt"), "resolved\n")?;
    git(dir, &["add", "f.txt"])?;
    let out = gs(dir, ["rebase", "--continue", "--json"])?;
    assert_eq!(out.status, Some(0), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["result"], "completed");
    assert_ne!(git(dir, &["rev-parse", "HEAD"])?.trim(), topic_tip);
    assert!(
        !dir.join(".git/rebase-merge/grit-plan").exists(),
        "session should be cleared"
    );
    Ok(())
}

#[test]
fn rebase_skip_json() -> TestResult {
    let scratch = Scratch::new("skip")?;
    let dir = scratch.path();
    let (upstream, onto, topic_tip, _) = init_conflict_rebase(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    gs(dir, ["rebase", "--onto", &onto, &upstream])?;
    let out = gs(dir, ["rebase", "--skip", "--json"])?;
    assert_eq!(out.status, Some(0), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["result"], "completed");
    assert_ne!(git(dir, &["rev-parse", "HEAD"])?.trim(), topic_tip);
    Ok(())
}

#[test]
fn rebase_abort_restores() -> TestResult {
    let scratch = Scratch::new("abort")?;
    let dir = scratch.path();
    let (upstream, onto, topic_tip, _) = init_conflict_rebase(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    gs(dir, ["rebase", "--onto", &onto, &upstream])?;
    let out = gs(dir, ["rebase", "--abort", "--json"])?;
    assert_eq!(out.status, Some(0), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["result"], "aborted");
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), topic_tip);
    assert_eq!(fs::read_to_string(dir.join("f.txt"))?, "topic\n");
    Ok(())
}

#[test]
fn status_shows_rebase_in_progress() -> TestResult {
    let scratch = Scratch::new("status")?;
    let dir = scratch.path();
    start_conflict_rebase(dir)?;
    let out = gs(dir, ["status", "--json"])?;
    assert_eq!(out.status, Some(0), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert!(
        json["in_progress"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "rebase")),
        "expected rebase in in_progress: {json}"
    );
    let op = json.get("operation").expect("operation block");
    assert_eq!(op["kind"], "rebase");
    assert!(op["next_steps"].is_array());
    assert!(op["alternatives"].is_array());
    Ok(())
}

#[test]
fn rebase_continue_without_state_errors() -> TestResult {
    let scratch = Scratch::new("no-state")?;
    let dir = scratch.path();
    init_repo(dir)?;
    let out = gs(dir, ["rebase", "--continue"])?;
    assert_ne!(out.status, Some(0));
    assert!(
        out.stderr.contains("no rebase in progress")
            || out.stdout.contains("no rebase in progress"),
        "stderr: {}\nstdout: {}",
        out.stderr,
        out.stdout
    );
    Ok(())
}

#[test]
fn rebase_conflict_markdown_snapshot() -> TestResult {
    let scratch = Scratch::new("md")?;
    let dir = scratch.path();
    let (upstream, onto, _, _) = init_conflict_rebase(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(dir, ["rebase", "--onto", &onto, &upstream, "--markdown"])?;
    assert_eq!(out.status, Some(1));
    assert!(out.stdout.contains("## Rebase"));
    assert!(out.stdout.contains("Rebase stopped with conflicts"));
    assert!(out.stdout.contains("**content** — `f.txt`"));
    assert!(out.stdout.contains("grit rebase --continue"));
    assert!(out.stdout.contains("grit rebase --abort"));
    assert!(out.stdout.contains("grit rebase --skip"));
    Ok(())
}

#[test]
fn restore_ours_theirs_works_during_rebase_stop() -> TestResult {
    let scratch = Scratch::new("restore")?;
    let dir = scratch.path();
    let (upstream, onto, _, file) = init_conflict_rebase(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    gs(dir, ["rebase", "--onto", &onto, &upstream])?;
    let out = gs(dir, ["restore", "--ours", &file, "--json"])?;
    assert_eq!(out.status, Some(0), "{}\n{}", out.stdout, out.stderr);
    assert!(!fs::read_to_string(dir.join(&file))?.contains("<<<<<<<"));
    let theirs_out = gs(dir, ["restore", "--theirs", &file, "--json"])?;
    assert_eq!(
        theirs_out.status,
        Some(0),
        "{}\n{}",
        theirs_out.stdout,
        theirs_out.stderr
    );
    Ok(())
}
