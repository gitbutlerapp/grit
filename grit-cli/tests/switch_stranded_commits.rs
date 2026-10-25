//! `grit switch` away from a detached `HEAD` reports the commits it leaves behind (issue #1852).

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

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
            "grit-cli-switch-stranded-{tag}-{}-{n}",
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

fn isolated(program: &str, dir: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x");
    cmd
}

fn run(program: &str, dir: &Path, args: &[impl AsRef<OsStr>]) -> Result<Output, Box<dyn Error>> {
    let out = isolated(program, dir).args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "{program} {:?} failed: {:?}\nstdout={}\nstderr={}",
            args.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(out)
}

fn gs(dir: &Path, args: &[&str]) -> Result<Output, Box<dyn Error>> {
    run(GS, dir, args)
}

fn git(dir: &Path, args: &[&str]) -> Result<Output, Box<dyn Error>> {
    run("git", dir, args)
}

fn rev_parse(dir: &Path, rev: &str) -> Result<String, Box<dyn Error>> {
    Ok(String::from_utf8(git(dir, &["rev-parse", rev])?.stdout)?
        .trim()
        .to_owned())
}

/// The issue's reproduction: one commit on `main`, one more on a detached `HEAD`.
fn repo_with_detached_commit(root: &Path) -> Result<String, Box<dyn Error>> {
    gs(root, &["init"])?;
    gs(root, &["config", "user.name", "T"])?;
    gs(root, &["config", "user.email", "t@e.x"])?;
    fs::write(root.join("a"), "a\n")?;
    gs(root, &["commit", "-m", "one"])?;
    gs(root, &["switch", "--detach", "HEAD"])?;
    fs::write(root.join("b"), "b\n")?;
    gs(root, &["commit", "-m", "work done on detached HEAD"])?;
    rev_parse(root, "HEAD")
}

#[test]
fn switch_json_reports_commit_stranded_on_detached_head() -> TestResult {
    let scratch = Scratch::new("json")?;
    let root = scratch.path();
    let stray = repo_with_detached_commit(root)?;

    let out = gs(root, &["switch", "main", "--json"])?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["branch"], "main");
    assert_eq!(json["stranded_total"], 1);
    assert!(json.get("stranded_truncated").is_none());
    let stranded = json["stranded_commits"]
        .as_array()
        .ok_or("stranded_commits missing")?;
    assert_eq!(stranded.len(), 1);
    assert_eq!(stranded[0]["oid"], stray.as_str());
    assert_eq!(stranded[0]["subject"], "work done on detached HEAD");
    assert!(stranded[0]["change_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert_eq!(rev_parse(root, "HEAD")?, rev_parse(root, "main")?);
    Ok(())
}

#[test]
fn switch_human_warns_with_a_command_that_keeps_the_commit() -> TestResult {
    let scratch = Scratch::new("human")?;
    let root = scratch.path();
    let stray = repo_with_detached_commit(root)?;

    let out = gs(root, &["switch", "main"])?;
    let stdout = String::from_utf8(out.stdout)?;
    let stderr = String::from_utf8(out.stderr)?;
    assert_eq!(stdout.trim(), "Switched to branch main");
    assert!(
        stderr.contains("Warning: 1 commit left behind, not on any branch:"),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains(&format!("{} work done on detached HEAD", &stray[..7])),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains(&format!("grit branch <name> {stray}")),
        "stderr={stderr}"
    );

    gs(root, &["branch", "rescued", &stray])?;
    assert_eq!(rev_parse(root, "rescued")?, stray);
    git(root, &["fsck", "--strict", "--no-dangling"])?;
    Ok(())
}

#[test]
fn switch_dry_run_markdown_reports_without_moving_head() -> TestResult {
    let scratch = Scratch::new("dry-run")?;
    let root = scratch.path();
    let stray = repo_with_detached_commit(root)?;

    let out = gs(root, &["switch", "main", "--dry-run", "--markdown"])?;
    let stdout = String::from_utf8(out.stdout)?;
    assert!(
        stdout.contains("1 commit would be left behind"),
        "stdout={stdout}"
    );
    assert!(stdout.contains(&stray), "stdout={stdout}");
    assert_eq!(rev_parse(root, "HEAD")?, stray);
    Ok(())
}

#[test]
fn switch_create_at_detached_head_strands_nothing() -> TestResult {
    let scratch = Scratch::new("create")?;
    let root = scratch.path();
    let stray = repo_with_detached_commit(root)?;

    let out = gs(root, &["switch", "-c", "keep", "--json"])?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["created"], true);
    assert!(json.get("stranded_commits").is_none(), "json={json}");
    assert!(json.get("stranded_total").is_none(), "json={json}");
    assert!(out.stderr.is_empty());
    assert_eq!(rev_parse(root, "keep")?, stray);
    Ok(())
}

#[test]
fn switch_off_a_branch_strands_nothing() -> TestResult {
    let scratch = Scratch::new("branch")?;
    let root = scratch.path();
    repo_with_detached_commit(root)?;
    gs(root, &["switch", "-c", "topic"])?;

    let out = gs(root, &["switch", "main", "--json"])?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert!(json.get("stranded_commits").is_none(), "json={json}");
    Ok(())
}

#[test]
fn switch_sees_commits_git_made_on_a_detached_head() -> TestResult {
    let scratch = Scratch::new("git-made")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"])?;
    fs::write(root.join("a"), "a\n")?;
    git(root, &["add", "a"])?;
    git(root, &["commit", "-qm", "one"])?;
    git(root, &["checkout", "-q", "--detach"])?;
    for (name, subject) in [("b", "two"), ("c", "three")] {
        fs::write(root.join(name), format!("{name}\n"))?;
        git(root, &["add", name])?;
        git(root, &["commit", "-qm", subject])?;
    }
    let tip = rev_parse(root, "HEAD")?;
    let parent = rev_parse(root, "HEAD~1")?;

    let out = gs(root, &["switch", "main", "--json"])?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["stranded_total"], 2);
    let oids: Vec<_> = json["stranded_commits"]
        .as_array()
        .ok_or("stranded_commits missing")?
        .iter()
        .map(|c| c["oid"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(oids, vec![tip, parent]);
    Ok(())
}
