//! Integration tests for `grit rebase`.

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

impl CmdOutput {
    fn success(&self) -> bool {
        self.status == Some(0)
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
        path.push(format!("grit-cli-rebase-{tag}-{}-{n}", std::process::id()));
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

fn git_fsck_strict(dir: &Path) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git fsck --strict failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

fn init_repo(dir: &Path) -> Result<(), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("f.txt"), "1\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    Ok(())
}

/// Classic rebase fixture: upstream at first commit, onto branch carries patch 2, topic has patch 3.
fn init_rebase_fixture(dir: &Path) -> Result<(String, String, String), Box<dyn Error>> {
    init_repo(dir)?;
    git(dir, &["commit", "-m", "second", "--allow-empty"])?;
    fs::write(dir.join("f.txt"), "2\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "second file"])?;
    fs::write(dir.join("f.txt"), "3\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "third"])?;
    let upstream = git(dir, &["rev-parse", "HEAD~2"])?.trim().to_string();
    git(dir, &["branch", "onto", &upstream])?;
    git(dir, &["checkout", "onto"])?;
    fs::write(dir.join("f.txt"), "2\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "onto second"])?;
    let onto = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "main"])?;
    Ok((
        upstream,
        onto,
        git(dir, &["rev-parse", "HEAD"])?.trim().to_string(),
    ))
}

#[test]
fn rebase_upstream_default_uses_tracking_branch() -> TestResult {
    let scratch = Scratch::new("upstream-default")?;
    let dir = scratch.path();
    init_repo(dir)?;
    git(dir, &["commit", "-m", "second", "--allow-empty"])?;
    git(dir, &["commit", "-m", "third", "--allow-empty"])?;
    let main_tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "-b", "topic", "HEAD~1"])?;
    git(dir, &["commit", "-m", "topic-only", "--allow-empty"])?;
    gs(dir, ["branch", "-u", "main"])?;
    let before = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let out = gs(dir, ["rebase", "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"], "rebase");
    let after = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_ne!(after, before);
    assert_eq!(
        git(dir, &["merge-base", &after, &main_tip])?.trim(),
        main_tip,
        "topic tip should sit on main after rebase"
    );
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_onto_json_shape() -> TestResult {
    let scratch = Scratch::new("json-shape")?;
    let dir = scratch.path();
    let (upstream, onto, _tip) = init_rebase_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(
        dir,
        ["rebase", "--onto", &onto, &upstream, "--dry-run", "--json"],
    )?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"], "rebase");
    assert_eq!(json["dry_run"], true);
    assert!(json.get("rewritten").is_some());
    assert!(json.get("refs").is_some());
    let rewritten = json["rewritten"].as_array().ok_or("rewritten array")?;
    assert!(
        rewritten.iter().all(|row| row.get("subject").is_some()),
        "each rewritten row must include subject: {json}"
    );
    let refs = json["refs"].as_array().ok_or("refs array")?;
    assert!(
        refs.iter()
            .all(|row| row.get("ref").is_some() && row.get("name").is_some()),
        "each ref row must include ref and name: {json}"
    );
    let mut keys: Vec<_> = json
        .as_object()
        .ok_or("expected object")?
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, ["dry_run", "head", "operation", "refs", "rewritten"]);
    Ok(())
}

#[test]
fn rebase_dry_run_moves_nothing() -> TestResult {
    let scratch = Scratch::new("dry-run")?;
    let dir = scratch.path();
    let (upstream, onto, tip) = init_rebase_fixture(dir)?;
    let out = gs(
        dir,
        ["rebase", "--onto", &onto, &upstream, "--dry-run", "--json"],
    )?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    Ok(())
}

#[test]
fn rebase_conflict_exits_1_and_changes_nothing() -> TestResult {
    let scratch = Scratch::new("conflict")?;
    let dir = scratch.path();
    init_repo(dir)?;
    git(dir, &["checkout", "-b", "topic"])?;
    fs::write(dir.join("conflict.txt"), "topic\n")?;
    git(dir, &["add", "conflict.txt"])?;
    git(dir, &["commit", "-m", "topic change"])?;
    let tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "main"])?;
    fs::write(dir.join("conflict.txt"), "main\n")?;
    git(dir, &["add", "conflict.txt"])?;
    git(dir, &["commit", "-m", "main change"])?;
    let onto = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let upstream = git(dir, &["rev-parse", "HEAD~1"])?.trim().to_string();
    git(dir, &["checkout", "topic"])?;
    let out = gs(dir, ["rebase", "--onto", &onto, &upstream, "--json"])?;
    assert_eq!(out.status, Some(1), "conflict must exit 1");
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["kind"], "conflict");
    assert!(json.get("commit").is_some());
    assert!(json.get("conflicts").is_some());
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    Ok(())
}

#[test]
fn rebase_result_matches_git_rebase() -> TestResult {
    let scratch_g = Scratch::new("match-grit")?;
    let scratch_git = Scratch::new("match-git")?;
    let (u_g, o_g, _) = init_rebase_fixture(scratch_g.path())?;
    let (u_git, o_git, _) = init_rebase_fixture(scratch_git.path())?;
    assert_eq!(u_g, u_git);
    assert_eq!(o_g, o_git);
    gs(scratch_g.path(), ["config", "grit.changeIds", "false"])?;
    let out = gs(scratch_g.path(), ["rebase", "--onto", &o_g, &u_g, "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    git(
        scratch_git.path(),
        &["rebase", "--onto", &o_git, &u_git, "main"],
    )?;
    let grit_tip = git(scratch_g.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let git_tip = git(scratch_git.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    assert_eq!(grit_tip, git_tip);
    let grit_tree = git(scratch_g.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    let git_tree = git(scratch_git.path(), &["rev-parse", "HEAD^{tree}"])?
        .trim()
        .to_string();
    assert_eq!(grit_tree, git_tree);
    git_fsck_strict(scratch_g.path())?;
    git_fsck_strict(scratch_git.path())?;
    Ok(())
}

#[test]
fn rebase_updates_stacked_branches() -> TestResult {
    let scratch = Scratch::new("stacked")?;
    let dir = scratch.path();
    init_repo(dir)?;
    git(dir, &["commit", "-m", "a", "--allow-empty"])?;
    git(dir, &["commit", "-m", "b", "--allow-empty"])?;
    git(dir, &["commit", "-m", "c", "--allow-empty"])?;
    git(dir, &["branch", "stacked", "HEAD~1"])?;
    let upstream = git(dir, &["rev-parse", "HEAD~2"])?.trim().to_string();
    git(dir, &["branch", "onto", &upstream])?;
    git(dir, &["checkout", "onto"])?;
    git(dir, &["commit", "-m", "onto tip", "--allow-empty"])?;
    let onto = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "main"])?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(dir, ["rebase", "--onto", &onto, &upstream, "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    let refs = json["refs"].as_array().ok_or("refs")?;
    assert!(
        refs.iter()
            .any(|r| r["ref"].as_str() == Some("refs/heads/stacked")),
        "stacked branch should move: {json}"
    );
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_exec_true_moves_refs() -> TestResult {
    let scratch = Scratch::new("exec-true")?;
    let dir = scratch.path();
    let (upstream, onto, tip) = init_rebase_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(
        dir,
        [
            "rebase", "--onto", &onto, &upstream, "--exec", "true", "--json",
        ],
    )?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"], "rebase");
    let exec = json["exec"].as_array().ok_or("exec array")?;
    assert!(!exec.is_empty());
    assert_ne!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_exec_failure_exit_code_and_refs_unchanged() -> TestResult {
    let scratch = Scratch::new("exec-fail")?;
    let dir = scratch.path();
    let (upstream, onto, tip) = init_rebase_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(
        dir,
        [
            "rebase", "--onto", &onto, &upstream, "--exec", "exit 7", "--json",
        ],
    )?;
    assert_eq!(out.status, Some(7), "stderr:\n{}", out.stderr);
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_exec_sees_each_commit_tree() -> TestResult {
    let scratch = Scratch::new("exec-log")?;
    let dir = scratch.path();
    let (upstream, onto, _tip) = init_rebase_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let log = dir.join("head.log");
    if log.exists() {
        fs::remove_file(&log)?;
    }
    let out = gs(
        dir,
        [
            "rebase",
            "--onto",
            &onto,
            &upstream,
            "--exec",
            "git rev-parse HEAD >> head.log",
            "--json",
        ],
    )?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    let rewritten = json["rewritten"].as_array().ok_or("rewritten")?;
    let new_ids: Vec<String> = rewritten
        .iter()
        .filter_map(|row| row["new"].as_str().map(str::to_owned))
        .collect();
    let logged = fs::read_to_string(&log)?;
    let lines: Vec<&str> = logged.lines().collect();
    assert_eq!(lines.len(), new_ids.len());
    for (line, expected) in lines.iter().zip(new_ids.iter()) {
        assert_eq!(*line, expected);
    }
    git_fsck_strict(dir)?;
    Ok(())
}
