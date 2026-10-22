//! Integration tests for `grit revert`.

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
        path.push(format!("grit-cli-revert-{tag}-{}-{n}", std::process::id()));
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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
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

fn init_repo(root: &Path) -> Result<(), Box<dyn Error>> {
    git(root, &["init", "-q", "-b", "main", "."])?;
    git(root, &["config", "user.email", "test@example.com"])?;
    git(root, &["config", "user.name", "Test User"])?;
    git(root, &["config", "gc.auto", "0"])?;
    Ok(())
}

fn commit_file(
    root: &Path,
    path: &str,
    contents: &str,
    msg: &str,
) -> Result<String, Box<dyn Error>> {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&full, contents)?;
    git(root, &["add", path])?;
    git(root, &["commit", "-qm", msg])?;
    Ok(git(root, &["rev-parse", "HEAD"])?.trim().to_owned())
}

fn git_fsck_strict(root: &Path) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(root)
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

#[test]
fn revert_creates_inverse_commit() -> TestResult {
    let scratch = Scratch::new("inverse")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;

    let parent_tree = git(repo, &["rev-parse", "HEAD~1^{tree}"])?
        .trim()
        .to_owned();

    let out = gs(repo, ["revert", &target])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stdout.contains("Reverted") && out.stdout.contains("Revert"),
        "{}",
        out.dump()
    );

    let head_tree = git(repo, &["rev-parse", "HEAD^{tree}"])?.trim().to_owned();
    assert_eq!(head_tree, parent_tree);
    assert_eq!(fs::read_to_string(repo.join("a.txt"))?, "v1\n");

    let body = git(repo, &["log", "-1", "--format=%B"])?;
    assert!(body.contains("Revert \"feature title\""));
    assert!(body.contains(&format!("This reverts commit {target}.")));

    git_fsck_strict(repo)?;
    Ok(())
}

#[test]
fn revert_conflict_leaves_markers() -> TestResult {
    let scratch = Scratch::new("conflict")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "base\n", "initial")?;
    let target = commit_file(repo, "a.txt", "feature\n", "feature")?;
    git(repo, &["reset", "--hard", "HEAD~1"])?;
    commit_file(repo, "a.txt", "manual\n", "manual")?;

    let out = gs(repo, ["revert", &target])?;
    assert_eq!(out.status, Some(1), "{}", out.dump());
    assert!(repo.join(".git/REVERT_HEAD").is_file());
    let wt = fs::read_to_string(repo.join("a.txt"))?;
    assert!(wt.contains("<<<<<<<"), "expected conflict markers: {wt}");
    Ok(())
}

#[test]
fn revert_json_fields() -> TestResult {
    let scratch = Scratch::new("json")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;

    let out = gs(repo, ["--json", "revert", &target])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let v: Value = serde_json::from_str(out.stdout.trim())?;
    assert_eq!(v["result"], "completed");
    assert_eq!(v["source"].as_str(), Some(target.as_str()));
    let oid = v["oid"].as_str().expect("oid");
    assert_eq!(oid.len(), 40);
    assert_eq!(v["subject"].as_str(), Some("Revert \"feature title\""));
    let commits = v["commits"].as_array().expect("commits");
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0]["source"].as_str(), Some(target.as_str()));
    assert_eq!(v["dropped"].as_array().map(|a| a.len()), Some(0));
    Ok(())
}

#[test]
fn revert_range_newest_first() -> TestResult {
    let scratch = Scratch::new("range")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "c1")?;
    commit_file(repo, "a.txt", "v2\n", "c2")?;
    commit_file(repo, "a.txt", "v3\n", "c3")?;
    let base = git(repo, &["rev-parse", "HEAD~2"])?.trim().to_owned();

    let git_wt = tempfile::tempdir()?;
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", repo.display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()?;
    git(
        git_wt.path(),
        &["revert", "--no-edit", &format!("{base}..HEAD")],
    )?;

    gs(repo, ["revert", &format!("{base}..HEAD")])?;
    assert_eq!(
        git(repo, &["rev-parse", "HEAD^{tree}"])?.trim(),
        git(git_wt.path(), &["rev-parse", "HEAD^{tree}"])?.trim()
    );
    Ok(())
}

#[test]
fn revert_merge_mainline_1_matches_git() -> TestResult {
    let scratch = Scratch::new("merge")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "f.txt", "a\n", "c1")?;
    git(repo, &["switch", "-c", "topic"])?;
    fs::write(repo.join("f.txt"), "t\n")?;
    git(repo, &["commit", "-qam", "topic"])?;
    git(repo, &["switch", "main"])?;
    commit_file(repo, "g.txt", "g\n", "prep")?;
    git(repo, &["merge", "--no-ff", "-qm", "Merge topic", "topic"])?;
    let merge = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    commit_file(repo, "after.txt", "after\n", "after")?;

    let git_wt = tempfile::tempdir()?;
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", repo.display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()?;
    git(git_wt.path(), &["revert", "-m", "1", "--no-edit", &merge])?;

    gs(repo, ["revert", "--mainline", "1", &merge])?;
    assert_eq!(
        git(repo, &["rev-parse", "HEAD^{tree}"])?.trim(),
        git(git_wt.path(), &["rev-parse", "HEAD^{tree}"])?.trim()
    );
    Ok(())
}

#[test]
fn revert_merge_without_mainline_errors_with_hint() -> TestResult {
    let scratch = Scratch::new("merge-hint")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "f.txt", "a\n", "c1")?;
    git(repo, &["switch", "-c", "topic"])?;
    fs::write(repo.join("f.txt"), "t\n")?;
    git(repo, &["commit", "-qam", "topic"])?;
    git(repo, &["switch", "main"])?;
    commit_file(repo, "g.txt", "g\n", "prep")?;
    git(repo, &["merge", "--no-ff", "-qm", "Merge topic", "topic"])?;
    let merge = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();

    let out = gs(repo, ["revert", &merge])?;
    assert_ne!(out.status, Some(0), "{}", out.dump());
    let msg = format!("{}{}", out.stderr, out.stdout);
    assert!(msg.contains("--mainline"), "{}", out.dump());
    Ok(())
}

#[test]
fn revert_conflict_continue() -> TestResult {
    let scratch = Scratch::new("continue")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "f.txt", "base\n", "base")?;
    git(repo, &["switch", "-c", "side"])?;
    fs::write(repo.join("f.txt"), "side\n")?;
    git(repo, &["commit", "-qam", "side"])?;
    let rev = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(repo, &["switch", "main"])?;
    fs::write(repo.join("f.txt"), "main\n")?;
    git(repo, &["commit", "-qam", "main"])?;

    let out = gs(repo, ["revert", &rev])?;
    assert_eq!(out.status, Some(1), "{}", out.dump());
    fs::write(repo.join("f.txt"), "base\n")?;
    git(repo, &["add", "f.txt"])?;
    let out = gs(repo, ["revert", "--continue"])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert_eq!(fs::read_to_string(repo.join("f.txt"))?, "base\n");
    Ok(())
}

#[test]
fn revert_abort() -> TestResult {
    let scratch = Scratch::new("abort")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "f.txt", "base\n", "base")?;
    git(repo, &["switch", "-c", "side"])?;
    fs::write(repo.join("f.txt"), "side\n")?;
    git(repo, &["commit", "-qam", "side"])?;
    let rev = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(repo, &["switch", "main"])?;
    fs::write(repo.join("f.txt"), "main\n")?;
    git(repo, &["commit", "-qam", "main"])?;
    let before = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();

    gs(repo, ["revert", &rev])?;
    let out = gs(repo, ["revert", "--abort"])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert_eq!(git(repo, &["rev-parse", "HEAD"])?.trim(), before);
    assert_eq!(fs::read_to_string(repo.join("f.txt"))?, "main\n");
    Ok(())
}

#[test]
fn revert_skip() -> TestResult {
    let scratch = Scratch::new("skip")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "f.txt", "base\n", "base")?;
    git(repo, &["switch", "-c", "topic"])?;
    fs::write(repo.join("f.txt"), "topic\n")?;
    git(repo, &["commit", "-qam", "topic-f"])?;
    let c1 = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(repo, &["switch", "main"])?;
    fs::write(repo.join("f.txt"), "main\n")?;
    git(repo, &["commit", "-qam", "main"])?;

    gs(repo, ["revert", &c1])?;
    let out = gs(repo, ["revert", "--skip"])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert_eq!(fs::read_to_string(repo.join("f.txt"))?, "main\n");
    Ok(())
}

#[test]
fn revert_message_flag() -> TestResult {
    let scratch = Scratch::new("message")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;

    let out = gs(
        repo,
        ["revert", "--message", "Custom revert subject", &target],
    )?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let subject = git(repo, &["log", "-1", "--format=%s"])?.trim().to_owned();
    assert_eq!(subject, "Custom revert subject");
    Ok(())
}

#[test]
fn revert_message_file() -> TestResult {
    let scratch = Scratch::new("message-file")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;
    let msg_path = repo.join("msg.txt");
    fs::write(&msg_path, "From file\n")?;

    let out = gs(
        repo,
        [
            "revert",
            "--message-file",
            msg_path.to_str().unwrap(),
            &target,
        ],
    )?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let subject = git(repo, &["log", "-1", "--format=%s"])?.trim().to_owned();
    assert_eq!(subject, "From file");
    Ok(())
}

fn commit_with_dates(
    root: &Path,
    path: &str,
    contents: &str,
    msg: &str,
    when: &str,
) -> Result<String, Box<dyn Error>> {
    let full = root.join(path);
    fs::write(&full, contents)?;
    git(root, &["add", path])?;
    let out = Command::new("git")
        .args(["commit", "-qm", msg])
        .current_dir(root)
        .env("GIT_AUTHOR_DATE", when)
        .env("GIT_COMMITTER_DATE", when)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!("commit failed: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(git(root, &["rev-parse", "HEAD"])?.trim().to_owned())
}

#[test]
fn revert_range_skewed_committer_dates_matches_git() -> TestResult {
    let scratch = Scratch::new("skew-dates")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_with_dates(repo, "f.txt", "0\n", "base", "@1000 +0000")?;
    let base = git(repo, &["rev-parse", "HEAD"])?.trim().to_owned();
    commit_with_dates(repo, "f.txt", "1\n", "c1", "@1200 +0000")?;
    commit_with_dates(repo, "f.txt", "2\n", "c2", "@1100 +0000")?;

    let git_wt = tempfile::tempdir()?;
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", repo.display()),
            &git_wt.path().to_string_lossy(),
        ])
        .status()?;
    git(
        git_wt.path(),
        &["revert", "--no-edit", &format!("{base}..HEAD")],
    )?;

    let out = gs(repo, ["revert", &format!("{base}..HEAD")])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert_eq!(
        fs::read_to_string(repo.join("f.txt"))?,
        fs::read_to_string(git_wt.path().join("f.txt"))?
    );
    Ok(())
}

#[test]
fn revert_json_schema() -> TestResult {
    let scratch = Scratch::new("schema")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;

    let out = gs(repo, ["--json", "revert", &target])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let v: Value = serde_json::from_str(out.stdout.trim())?;
    assert!(v.get("result").is_some());
    assert!(v.get("commits").is_some());
    assert!(v.get("dropped").is_some());
    assert!(v.get("source").is_some());
    assert!(v.get("oid").is_some());
    Ok(())
}
