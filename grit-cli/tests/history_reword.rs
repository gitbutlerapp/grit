//! Integration tests for `grit history reword`.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
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
        path.push(format!(
            "grit-cli-history-reword-{tag}-{}-{n}",
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

fn git_version_at_least(major: u32, minor: u32) -> bool {
    let out = Command::new("git").arg("--version").output().ok();
    let Some(out) = out else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(rest) = text.strip_prefix("git version ") else {
        return false;
    };
    let mut parts = rest.split('.');
    let maj: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (maj, min) >= (major, minor)
}

fn init_linear_repo(dir: &Path) -> Result<(String, String, String), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("file.txt"), b"v1\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    let a = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("file.txt"), b"v2\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "second"])?;
    let b = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("file.txt"), b"v3\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "third"])?;
    let c = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok((a, b, c))
}

fn parse_json(stdout: &str) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(stdout.trim())?)
}

#[test]
fn history_reword_matches_git_commit_tree_reconstruction() -> TestResult {
    let scratch = Scratch::new("tree")?;
    let dir = scratch.path();
    let (a, b, _c) = init_linear_repo(dir)?;
    let out = gs(
        dir,
        [
            "history",
            "reword",
            &b,
            "-m",
            "second reworded",
            "--update-refs",
            "head",
            "--json",
        ],
    )?;
    assert!(out.success(), "grit failed: {}", out.stderr);
    let json = parse_json(&out.stdout)?;
    let rewritten = json["rewritten"].as_array().ok_or("missing rewritten")?;
    let b_row = rewritten
        .iter()
        .find(|row| row["old"].as_str() == Some(b.as_str()))
        .ok_or("missing b in rewritten")?;
    let new_b = b_row["new"].as_str().ok_or("missing new b")?;
    let raw = Command::new("git")
        .args(["cat-file", "commit", new_b])
        .current_dir(dir)
        .output()?;
    assert!(raw.status.success());
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(["hash-object", "-w", "-t", "commit", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    child.stdin.as_mut().unwrap().write_all(&raw.stdout)?;
    let hash_out = child.wait_with_output()?;
    assert!(hash_out.status.success());
    let git_oid = String::from_utf8_lossy(&hash_out.stdout).trim().to_string();
    assert_eq!(new_b, git_oid);
    let _ = a;
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn history_reword_matches_git_history_when_available() -> TestResult {
    if !git_version_at_least(2, 54) {
        eprintln!("skipping: system git < 2.54");
        return Ok(());
    }
    let grit_dir = Scratch::new("git-parity-grit")?;
    let git_dir = Scratch::new("git-parity-git")?;
    let (_a1, b1, c1) = init_linear_repo(grit_dir.path())?;
    let (_a2, b2, _c2) = init_linear_repo(git_dir.path())?;
    assert_eq!(b1, b2);
    git(grit_dir.path(), &["config", "grit.changeIds", "false"])?;
    let msg = "parity reword\n";
    let msg_file = grit_dir.path().join("msg.txt");
    fs::write(&msg_file, msg)?;
    let grit_out = gs(
        grit_dir.path(),
        [
            "history",
            "reword",
            &b1,
            "--message-file",
            msg_file.to_str().unwrap(),
        ],
    )?;
    assert!(grit_out.success(), "{}", grit_out.stderr);
    let editor = grit_dir.path().join("write-msg.sh");
    fs::write(
        &editor,
        format!("#!/bin/sh\ncp '{}' \"$1\"\n", msg_file.to_string_lossy()),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&editor, fs::Permissions::from_mode(0o755))?;
    }
    let git_out = Command::new("git")
        .current_dir(git_dir.path())
        .args(["history", "reword", &b2])
        .env("GIT_EDITOR", editor.to_string_lossy().as_ref())
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(
        git_out.status.success(),
        "git history reword: {}",
        String::from_utf8_lossy(&git_out.stderr)
    );
    let grit_tip = git(grit_dir.path(), &["rev-parse", "main"])?
        .trim()
        .to_string();
    let git_tip = git(git_dir.path(), &["rev-parse", "main"])?
        .trim()
        .to_string();
    assert_eq!(grit_tip, git_tip);
    let grit_log = git(grit_dir.path(), &["rev-list", "--reverse", "main"])?;
    let git_log = git(git_dir.path(), &["rev-list", "--reverse", "main"])?;
    assert_eq!(grit_log, git_log);
    let _ = c1;
    git_fsck_strict(grit_dir.path())?;
    git_fsck_strict(git_dir.path())?;
    Ok(())
}

#[test]
fn history_reword_dry_run_changes_no_refs() -> TestResult {
    let scratch = Scratch::new("dry")?;
    let dir = scratch.path();
    let (_a, b, c) = init_linear_repo(dir)?;
    let before_main = git(dir, &["rev-parse", "main"])?.trim().to_string();
    let before_head = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let out = gs(
        dir,
        [
            "history",
            "reword",
            &b,
            "-m",
            "dry only",
            "--dry-run",
            "--json",
        ],
    )?;
    assert!(out.success(), "{}", out.stderr);
    let after_main = git(dir, &["rev-parse", "main"])?.trim().to_string();
    let after_head = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_eq!(before_main, after_main);
    assert_eq!(before_head, after_head);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["dry_run"], true);
    assert!(json["refs"].as_array().is_some_and(|a| !a.is_empty()));
    let _ = c;
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn history_reword_update_refs_head() -> TestResult {
    let scratch = Scratch::new("head-scope")?;
    let dir = scratch.path();
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("t.txt"), b"1\n")?;
    git(dir, &["add", "t.txt"])?;
    git(dir, &["commit", "-m", "one"])?;
    let a = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("t.txt"), b"2\n")?;
    git(dir, &["add", "t.txt"])?;
    git(dir, &["commit", "-m", "two"])?;
    git(dir, &["branch", "side"])?;
    git(dir, &["checkout", "main"])?;
    fs::write(dir.join("t.txt"), b"3\n")?;
    git(dir, &["add", "t.txt"])?;
    git(dir, &["commit", "-m", "three"])?;
    let b = git(dir, &["rev-parse", "HEAD~1"])?.trim().to_string();
    let side_before = git(dir, &["rev-parse", "side"])?.trim().to_string();
    let out = gs(
        dir,
        [
            "history",
            "reword",
            &b,
            "-m",
            "two reworded",
            "--update-refs",
            "head",
        ],
    )?;
    assert!(out.success(), "{}", out.stderr);
    let side_after = git(dir, &["rev-parse", "side"])?.trim().to_string();
    assert_eq!(side_before, side_after, "side branch must be untouched");
    let main_tip = git(dir, &["rev-parse", "main"])?.trim().to_string();
    assert_ne!(main_tip, side_before);
    let _ = a;
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn history_reword_json_head_null_when_head_unchanged() -> TestResult {
    let scratch = Scratch::new("head-null")?;
    let dir = scratch.path();
    let (_a, b, c) = init_linear_repo(dir)?;
    git(dir, &["branch", "side", &b])?;
    fs::write(dir.join("side.txt"), b"side\n")?;
    git(dir, &["checkout", "side"])?;
    git(dir, &["add", "side.txt"])?;
    git(dir, &["commit", "-m", "side tip"])?;
    let side_tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "main"])?;
    let head_before = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_eq!(head_before, c);
    let out = gs(
        dir,
        [
            "history",
            "reword",
            &side_tip,
            "-m",
            "side reworded",
            "--json",
        ],
    )?;
    assert!(out.success(), "{}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert!(
        json.get("head").is_some(),
        "head key must be present: {json}"
    );
    assert!(
        json["head"].is_null(),
        "head must be null when HEAD commit is unchanged: {json}"
    );
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), head_before);
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn history_reword_json_schema() -> TestResult {
    let scratch = Scratch::new("schema")?;
    let dir = scratch.path();
    let (_a, b, _c) = init_linear_repo(dir)?;
    let out = gs(
        dir,
        ["history", "reword", &b, "-m", "schema test", "--json"],
    )?;
    assert!(out.success(), "{}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"], "reword");
    assert!(json.get("rewritten").is_some());
    assert!(json.get("refs").is_some());
    assert_eq!(json["dry_run"], false);
    let rewritten = json["rewritten"].as_array().ok_or("rewritten array")?;
    assert!(
        rewritten.iter().all(|row| row.get("subject").is_some()),
        "each rewritten row must include subject: {json}"
    );
    assert!(
        rewritten
            .iter()
            .any(|row| row["subject"].as_str() == Some("schema test")),
        "reworded subject in JSON: {json}"
    );
    let refs = json["refs"].as_array().ok_or("refs array")?;
    assert!(
        refs.iter()
            .all(|row| row.get("ref").is_some() && row.get("name").is_some()),
        "each ref row must include ref and deprecated name: {json}"
    );
    let mut keys: Vec<_> = json
        .as_object()
        .ok_or("expected object")?
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, ["dry_run", "head", "operation", "refs", "rewritten"]);
    git_fsck_strict(dir)?;
    Ok(())
}
