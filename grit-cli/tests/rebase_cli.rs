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

/// Like [`init_rebase_fixture`], but `stable.txt` is committed once and never changed on topic or onto.
fn init_rebase_fixture_with_stable(dir: &Path) -> Result<(String, String, String), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("f.txt"), "1\n")?;
    fs::write(dir.join("stable.txt"), "stable\n")?;
    git(dir, &["add", "f.txt", "stable.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
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
    assert_eq!(json["head"]["old"], before);
    assert_eq!(json["head"]["new"], after);
    assert_eq!(
        git(dir, &["merge-base", &after, &main_tip])?.trim(),
        main_tip,
        "topic tip should sit on main after rebase"
    );
    let out = gs(dir, ["rebase", "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    assert!(parse_json(&out.stdout)?["head"].is_null());
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), after);
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
    assert_eq!(
        keys,
        [
            "dry_run",
            "head",
            "operation",
            "refs",
            "result",
            "rewritten"
        ]
    );
    assert_eq!(json["result"], "completed");
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
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["head"]["old"], tip);
    assert_ne!(json["head"]["new"], json["head"]["old"]);
    assert_eq!(
        json["head"]["new"],
        json["rewritten"]
            .as_array()
            .ok_or("rewritten array")?
            .last()
            .ok_or("rewritten tip")?["new"]
    );
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
    assert_eq!(json["result"], "conflicts");
    assert!(json.get("commit").is_some());
    assert!(json.get("conflicts").is_some());
    assert!(json.get("next_steps").is_some());
    let orig_head = fs::read_to_string(dir.join(".git/rebase-merge/orig-head"))?
        .trim()
        .to_string();
    assert_eq!(
        orig_head, tip,
        "orig-head must record the pre-rebase branch tip"
    );
    assert!(
        dir.join(".git/rebase-merge/grit-plan").is_file(),
        "grit rebase should leave resumable state"
    );
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
fn rebase_main_moves_deep_stacked_branches_json() -> TestResult {
    let scratch = Scratch::new("deep-stack")?;
    let dir = scratch.path();
    init_repo(dir)?;
    git(dir, &["commit", "-m", "base", "--allow-empty"])?;
    for branch in ["a", "b", "c"] {
        git(dir, &["switch", "-q", "-c", branch])?;
        for n in 1..=2 {
            git(
                dir,
                &["commit", "-m", &format!("{branch}{n}"), "--allow-empty"],
            )?;
        }
    }
    git(dir, &["switch", "-q", "main"])?;
    git(dir, &["commit", "-m", "main advances", "--allow-empty"])?;
    git(dir, &["switch", "-q", "a"])?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(dir, ["rebase", "main", "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    let refs: Vec<&str> = json["refs"]
        .as_array()
        .ok_or("refs")?
        .iter()
        .filter_map(|r| r["ref"].as_str())
        .collect();
    for name in ["refs/heads/a", "refs/heads/b", "refs/heads/c"] {
        assert!(refs.contains(&name), "missing {name} in {json}");
    }
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_keeps_backup_branch_at_old_tip() -> TestResult {
    let scratch = Scratch::new("backup")?;
    let dir = scratch.path();
    init_repo(dir)?;
    git(dir, &["switch", "-q", "-c", "feature"])?;
    fs::write(dir.join("x.txt"), "x\n")?;
    git(dir, &["add", "x.txt"])?;
    git(dir, &["commit", "-m", "X"])?;
    fs::write(dir.join("y.txt"), "y\n")?;
    git(dir, &["add", "y.txt"])?;
    git(dir, &["commit", "-m", "Y"])?;
    git(dir, &["branch", "backup"])?;
    let old_tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["switch", "-q", "main"])?;
    fs::write(dir.join("m.txt"), "m\n")?;
    git(dir, &["add", "m.txt"])?;
    git(dir, &["commit", "-m", "M"])?;
    git(dir, &["switch", "-q", "feature"])?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(dir, ["rebase", "main", "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    let refs: Vec<&str> = json["refs"]
        .as_array()
        .ok_or("refs")?
        .iter()
        .filter_map(|r| r["ref"].as_str())
        .collect();
    assert_eq!(refs, ["refs/heads/feature"], "{json}");
    assert_eq!(git(dir, &["rev-parse", "backup"])?.trim(), old_tip);
    assert_eq!(
        git(dir, &["rev-parse", "feature~2"])?,
        git(dir, &["rev-parse", "main"])?
    );
    git_fsck_strict(dir)?;
    Ok(())
}

fn init_rebase_conflict_fixture(dir: &Path) -> Result<(String, String), Box<dyn Error>> {
    init_repo(dir)?;
    git(dir, &["checkout", "-b", "topic"])?;
    fs::write(dir.join("conflict.txt"), "topic\n")?;
    git(dir, &["add", "conflict.txt"])?;
    git(dir, &["commit", "-m", "topic"])?;
    git(dir, &["checkout", "main"])?;
    fs::write(dir.join("conflict.txt"), "main\n")?;
    git(dir, &["add", "conflict.txt"])?;
    git(dir, &["commit", "-m", "main"])?;
    let onto = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["checkout", "topic"])?;
    let upstream = git(dir, &["rev-parse", "HEAD~1"])?.trim().to_string();
    Ok((upstream, onto))
}

#[test]
fn rebase_check_succeeds_when_gpgsign_enabled() -> TestResult {
    let scratch = Scratch::new("check-gpgsign")?;
    let dir = scratch.path();
    let (upstream, onto) = init_rebase_conflict_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    gs(dir, ["config", "commit.gpgsign", "true"])?;
    let out = gs(
        dir,
        ["rebase", "--check", "--onto", &onto, &upstream, "--json"],
    )?;
    assert_eq!(
        out.status,
        Some(1),
        "expected conflict prediction, not signing failure: {}\n{}",
        out.stdout,
        out.stderr
    );
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    Ok(())
}

#[test]
fn rebase_check_exit_codes() -> TestResult {
    let scratch = Scratch::new("check")?;
    let dir = scratch.path();
    let (upstream, onto) = init_rebase_conflict_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let head_before = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    let conflict = gs(
        dir,
        ["rebase", "--check", "--onto", &onto, &upstream, "--json"],
    )?;
    assert_eq!(conflict.status, Some(1));
    let json = parse_json(&conflict.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert_eq!(json["check"], true);
    assert!(json["conflicts"].as_array().is_some_and(|a| !a.is_empty()));
    let head_after = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_eq!(head_before, head_after);

    let base = upstream.clone();
    let clean = gs(dir, ["rebase", "--check", &base, "--json"])?;
    assert_eq!(clean.status, Some(0), "{}\n{}", clean.stdout, clean.stderr);
    let clean_json = parse_json(&clean.stdout)?;
    assert_eq!(clean_json["result"], "up_to_date");
    Ok(())
}

#[test]
fn rebase_x_theirs_json() -> TestResult {
    let scratch = Scratch::new("x-theirs")?;
    let dir = scratch.path();
    let (upstream, onto) = init_rebase_conflict_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    let out = gs(
        dir,
        [
            "rebase", "-X", "theirs", "--onto", &onto, &upstream, "--json",
        ],
    )?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    assert_ne!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    assert_eq!(
        git(dir, &["rev-parse", "--verify", "ORIG_HEAD"])?.trim(),
        tip
    );
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"], "rebase");
    assert!(json["rewritten"].as_array().is_some_and(|a| !a.is_empty()));
    let content = git(dir, &["show", "HEAD:conflict.txt"])?.trim().to_string();
    assert_eq!(content, "topic");
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
    assert_eq!(
        git(dir, &["rev-parse", "--verify", "ORIG_HEAD"])?.trim(),
        tip
    );
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_exec_failure_emits_single_error_line() -> TestResult {
    let scratch = Scratch::new("exec-fail-once")?;
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
    assert!(
        git(dir, &["rev-parse", "--verify", "-q", "ORIG_HEAD"]).is_err(),
        "a rolled-back exec rebase records no ORIG_HEAD"
    );
    let marker = "rebase --exec command failed";
    let count = out.stderr.matches(marker).count() + out.stdout.matches(marker).count();
    assert_eq!(
        count, 1,
        "exec failure message must appear once, got {count} in:\nstdout:{}\nstderr:{}",
        out.stdout, out.stderr
    );
    Ok(())
}

#[test]
fn rebase_exec_failure_restores_unchanged_tracked_file() -> TestResult {
    let scratch = Scratch::new("exec-stable-restore")?;
    let dir = scratch.path();
    let (upstream, onto, tip) = init_rebase_fixture_with_stable(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(
        dir,
        [
            "rebase",
            "--onto",
            &onto,
            &upstream,
            "--exec",
            "printf 'dirty\\n' >> stable.txt; exit 7",
        ],
    )?;
    assert_eq!(out.status, Some(7), "stderr:\n{}", out.stderr);
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    assert!(
        git(dir, &["rev-parse", "--verify", "-q", "ORIG_HEAD"]).is_err(),
        "a rolled-back exec rebase records no ORIG_HEAD"
    );
    assert_eq!(
        fs::read_to_string(dir.join("stable.txt"))?,
        "stable\n",
        "stable.txt must match pre-rebase contents"
    );
    let porcelain = git(dir, &["status", "--porcelain"])?.trim().to_string();
    assert!(
        porcelain.is_empty(),
        "git status must be clean after exec rollback, got:\n{porcelain}"
    );
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
    assert!(
        git(dir, &["rev-parse", "--verify", "-q", "ORIG_HEAD"]).is_err(),
        "a rolled-back exec rebase records no ORIG_HEAD"
    );
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn rebase_exec_forwards_stdout_and_stderr() -> TestResult {
    let scratch = Scratch::new("exec-streams")?;
    let dir = scratch.path();
    let (upstream, onto, tip) = init_rebase_fixture(dir)?;
    gs(dir, ["config", "grit.changeIds", "false"])?;
    let out = gs(
        dir,
        [
            "rebase",
            "--onto",
            &onto,
            &upstream,
            "--exec",
            "echo EXEC_STDOUT; echo EXEC_STDERR >&2",
            "--exec",
            "exit 7",
            "--json",
        ],
    )?;
    assert_eq!(out.status, Some(7), "stderr:\n{}", out.stderr);
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    assert!(
        git(dir, &["rev-parse", "--verify", "-q", "ORIG_HEAD"]).is_err(),
        "a rolled-back exec rebase records no ORIG_HEAD"
    );
    assert!(
        out.stderr.contains("EXEC_STDOUT"),
        "stdout from successful --exec must be forwarded: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("EXEC_STDERR"),
        "stderr from --exec must be forwarded: {}",
        out.stderr
    );
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

/// The issue #2032 fixture: an unchanged tracked file is edited by a successful exec.
fn init_dirty_exec_fixture(dir: &Path, count: usize) -> Result<String, Box<dyn Error>> {
    git(dir, &["init", "-q", "-b", "main"])?;
    fs::write(dir.join("f"), "base\n")?;
    git(dir, &["add", "f"])?;
    git(dir, &["commit", "-qm", "base"])?;
    git(dir, &["switch", "-qc", "feature"])?;
    for step in 1..=count {
        let path = format!("step-{step}");
        fs::write(dir.join(&path), format!("{step}\n"))?;
        git(dir, &["add", &path])?;
        git(dir, &["commit", "-qm", &path])?;
    }
    let tip = git(dir, &["rev-parse", "feature"])?.trim().to_owned();
    git(dir, &["switch", "-q", "main"])?;
    fs::write(dir.join("extra"), "upstream\n")?;
    git(dir, &["add", "extra"])?;
    git(dir, &["commit", "-qm", "main moves"])?;
    git(dir, &["switch", "-q", "feature"])?;
    Ok(tip)
}

#[test]
fn successful_dirty_exec_stops_immediately_and_preserves_index_and_worktree() -> TestResult {
    for history_alias in [false, true] {
        for (command, expected_file, expected_index, expected_status) in [
            (
                "echo hook-edit >> f",
                Some("base\nhook-edit\n"),
                Some("base\n"),
                " M f\n",
            ),
            (
                "echo hook-edit >> f; git add f",
                Some("base\nhook-edit\n"),
                Some("base\nhook-edit\n"),
                "M  f\n",
            ),
            (
                "echo staged >> f; git add f; echo unstaged >> f",
                Some("base\nstaged\nunstaged\n"),
                Some("base\nstaged\n"),
                "MM f\n",
            ),
            ("rm f", None, Some("base\n"), " D f\n"),
            (
                "git rm -q f; echo new > added; git add added",
                None,
                None,
                "A  added\nD  f\n",
            ),
        ] {
            for (count, dirty_at) in [(1, 1), (3, 1), (3, 2), (3, 3)] {
                let scratch = Scratch::new("exec-dirty-preserved")?;
                let dir = scratch.path();
                let tip = init_dirty_exec_fixture(dir, count)?;
                let upstream = git(dir, &["rev-parse", "main"])?;
                let conditional = format!("if test -f step-{dirty_at} && ! test -f step-{}; then {command}; echo EXEC_OUT; echo EXEC_ERR >&2; fi", dirty_at + 1);
                let mut args = vec![];
                if history_alias {
                    args.push("history");
                }
                args.extend([
                    "rebase",
                    "main",
                    "--exec",
                    &conditional,
                    "--exec",
                    "echo next-command >> ran-next",
                    "--json",
                ]);
                let out = gs(dir, args)?;
                assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
                let json = parse_json(&out.stdout)?;
                assert_eq!(json["result"], "exec_dirty");
                assert_eq!(json["command"], conditional);
                let expected_paths: Vec<_> =
                    expected_status.lines().map(|line| &line[3..]).collect();
                assert_eq!(json["paths"], serde_json::json!(expected_paths));
                assert_eq!(
                    json["exec"].as_array().ok_or("exec")?.len(),
                    (dirty_at - 1) * 2 + 1
                );
                assert_eq!(json["commit"], git(dir, &["rev-parse", "HEAD"])?.trim());
                assert!(out.stderr.contains("EXEC_OUT") && out.stderr.contains("EXEC_ERR"));
                assert_eq!(git(dir, &["rev-parse", "feature"])?.trim(), tip);
                assert_eq!(git(dir, &["rev-parse", "main"])?, upstream);
                assert_eq!(
                    fs::read_to_string(dir.join("f")).ok().as_deref(),
                    expected_file
                );
                let index_file = Command::new("git")
                    .args(["show", ":f"])
                    .current_dir(dir)
                    .output()?;
                assert_eq!(index_file.status.success(), expected_index.is_some());
                if let Some(expected) = expected_index {
                    assert_eq!(String::from_utf8(index_file.stdout)?, expected);
                }
                // The untracked exec log must contain only the earlier clean invocations.
                assert_eq!(
                    fs::read_to_string(dir.join("ran-next"))
                        .unwrap_or_default()
                        .lines()
                        .count(),
                    dirty_at - 1
                );
                let status = git(dir, &["status", "--short", "--untracked-files=no"])?;
                assert_eq!(status, expected_status);
                assert_eq!(
                    fs::read_to_string(dir.join(".git/rebase-merge/orig-head"))?.trim(),
                    tip
                );
                assert!(dir.join(".git/rebase-merge/grit-exec").exists());
                let resume = gs(dir, ["rebase", "--continue", "--json"])?;
                assert_eq!(resume.status, Some(1), "dirty continue must refuse");
                assert_eq!(
                    fs::read_to_string(dir.join("f")).ok().as_deref(),
                    expected_file
                );
                assert_eq!(
                    git(dir, &["status", "--short", "--untracked-files=no"])?,
                    status
                );
                git_fsck_strict(dir)?;
                let abort = gs(dir, ["rebase", "--abort", "--json"])?;
                assert!(abort.success(), "{}\n{}", abort.stdout, abort.stderr);
                assert_eq!(fs::read_to_string(dir.join("f"))?, "base\n");
                assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
                assert!(!dir.join(".git/rebase-merge").exists());
                git_fsck_strict(dir)?;
            }
        }
    }
    Ok(())
}

#[test]
fn exec_stop_with_moved_branch_refuses_resume_and_abort_restores_the_branch() -> TestResult {
    for mode in ["human", "--json", "--markdown"] {
        let scratch = Scratch::new("exec-dirty-moved-branch")?;
        let dir = scratch.path();
        let tip = init_dirty_exec_fixture(dir, 2)?;
        let stop = gs(dir, ["rebase", "main", "--exec", "echo fmt >> f"])?;
        assert_eq!(stop.status, Some(1), "{}\n{}", stop.stdout, stop.stderr);
        let moved = git(
            dir,
            &[
                "commit-tree",
                "-p",
                "feature",
                "-m",
                "moved",
                "feature^{tree}",
            ],
        )?
        .trim()
        .to_owned();
        git(dir, &["update-ref", "refs/heads/feature", &moved])?;

        let flags: &[&str] = if mode == "human" { &[] } else { &[mode] };
        for resume in ["--continue", "--skip"] {
            let out = gs(dir, [&["rebase", resume][..], flags].concat())?;
            assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
            let text = format!("{}{}", out.stdout, out.stderr);
            assert!(
                text.contains("refs/heads/feature moved to")
                    && text.contains("grit rebase --abort")
                    && !text.contains("exec left tracked changes"),
                "{resume} {mode}: {text}"
            );
            assert_eq!(git(dir, &["rev-parse", "feature"])?.trim(), moved);
            assert_eq!(fs::read_to_string(dir.join("f"))?, "base\nfmt\n");
        }

        let abort = gs(dir, [&["rebase", "--abort"][..], flags].concat())?;
        assert!(abort.success(), "{}\n{}", abort.stdout, abort.stderr);
        match mode {
            "--json" => {
                let json = parse_json(&abort.stdout)?;
                assert_eq!(json["result"], "aborted");
                assert_eq!(
                    json["refs"],
                    serde_json::json!([{
                        "ref": "refs/heads/feature",
                        "name": "refs/heads/feature",
                        "old": moved,
                        "new": tip,
                    }])
                );
            }
            _ => {
                assert!(abort.stdout.contains("Rebase aborted."), "{}", abort.stdout);
                assert!(
                    abort.stdout.contains("refs/heads/feature")
                        && abort.stdout.contains(&moved[..7])
                        && abort.stdout.contains(&tip[..7]),
                    "{}",
                    abort.stdout
                );
            }
        }
        assert_eq!(git(dir, &["rev-parse", "feature"])?.trim(), tip);
        assert_eq!(
            git(dir, &["symbolic-ref", "HEAD"])?.trim(),
            "refs/heads/feature"
        );
        assert_eq!(git(dir, &["rev-parse", "feature@{1}"])?.trim(), moved);
        assert_eq!(fs::read_to_string(dir.join("f"))?, "base\n");
        assert_eq!(git(dir, &["status", "--porcelain"])?, "");
        assert!(!dir.join(".git/rebase-merge").exists());
        git_fsck_strict(dir)?;
        let again = gs(dir, ["rebase", "main", "--json"])?;
        assert!(again.success(), "{}\n{}", again.stdout, again.stderr);
    }
    Ok(())
}

/// The issue #2156 fixture: `one` and `two` on `feature`, `up` on `main`.
fn init_amend_exec_fixture(dir: &Path) -> Result<(), Box<dyn Error>> {
    git(dir, &["init", "-q", "-b", "main"])?;
    for (path, branch) in [("f", None), ("one", Some("feature")), ("two", None)] {
        if let Some(branch) = branch {
            git(dir, &["switch", "-qc", branch])?;
        }
        fs::write(dir.join(path), format!("{path}\n"))?;
        git(dir, &["add", path])?;
        git(dir, &["commit", "-qm", path])?;
    }
    git(dir, &["switch", "-q", "main"])?;
    fs::write(dir.join("up"), "up\n")?;
    git(dir, &["add", "up"])?;
    git(dir, &["commit", "-qm", "up"])?;
    git(dir, &["switch", "-q", "feature"])?;
    Ok(())
}

#[test]
fn exec_that_amends_each_commit_completes_like_git() -> TestResult {
    let command = "echo fmt >> one && git commit -qa --amend --no-edit";
    let scratch = Scratch::new("exec-amend")?;
    let dir = scratch.path();
    init_amend_exec_fixture(dir)?;
    let out = gs(dir, ["rebase", "main", "--exec", command, "--json"])?;
    assert!(out.success(), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["result"], "completed");
    let tip = git(dir, &["rev-parse", "feature"])?.trim().to_owned();
    assert_eq!(json["head"]["new"], tip);
    assert_eq!(json["rewritten"].as_array().ok_or("rewritten")?.len(), 2);
    assert_eq!(json["exec"].as_array().ok_or("exec")?.len(), 2);
    assert_eq!(
        git(dir, &["log", "--format=%s", "feature"])?,
        "two\none\nup\nf\n"
    );
    assert_eq!(git(dir, &["show", "feature~1:one"])?, "one\nfmt\n");
    assert_eq!(
        git(dir, &["symbolic-ref", "HEAD"])?.trim(),
        "refs/heads/feature"
    );
    assert_eq!(git(dir, &["status", "--porcelain"])?, "");
    assert!(!dir.join(".git/rebase-merge").exists());
    git_fsck_strict(dir)?;

    let twin = Scratch::new("exec-amend-git")?;
    init_amend_exec_fixture(twin.path())?;
    git(twin.path(), &["rebase", "-q", "main", "--exec", command])?;
    for rev in ["feature^{tree}", "feature~1^{tree}"] {
        assert_eq!(
            git(dir, &["rev-parse", rev])?,
            git(twin.path(), &["rev-parse", rev])?
        );
    }
    Ok(())
}

#[test]
fn exec_amend_conflict_stops_with_conflicts_and_continues() -> TestResult {
    let command = "if ! test -f two; then echo amended > two && git add two && git commit -q --amend --no-edit; fi";
    let scratch = Scratch::new("exec-amend-conflict")?;
    let dir = scratch.path();
    init_amend_exec_fixture(dir)?;
    let tip = git(dir, &["rev-parse", "feature"])?.trim().to_owned();
    let out = gs(dir, ["rebase", "main", "--exec", command, "--json"])?;
    assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert_eq!(json["commit"], tip);
    assert_eq!(json["conflicts"][0]["path"], "two");
    assert_eq!(json["exec"].as_array().ok_or("exec")?.len(), 1);
    assert_eq!(git(dir, &["rev-parse", "feature"])?.trim(), tip);
    fs::write(dir.join("two"), "resolved\n")?;
    git(dir, &["add", "two"])?;
    let resume = gs(dir, ["rebase", "--continue", "--json"])?;
    assert!(resume.success(), "{}\n{}", resume.stdout, resume.stderr);
    assert_eq!(parse_json(&resume.stdout)?["result"], "completed");
    assert_eq!(git(dir, &["show", "feature:two"])?, "resolved\n");
    assert_eq!(git(dir, &["show", "feature~1:two"])?, "amended\n");
    assert_eq!(git(dir, &["status", "--porcelain"])?, "");
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn exec_conflict_continue_keeps_the_empty_policy_the_rebase_started_with() -> TestResult {
    let command = "if ! test -f two; then echo amended > two && git add two && git commit -q --amend --no-edit; fi";
    let scratch = Scratch::new("exec-resume-options")?;
    let dir = scratch.path();
    init_amend_exec_fixture(dir)?;
    git(dir, &["switch", "-q", "feature"])?;
    fs::write(dir.join("two"), "C\n")?;
    git(dir, &["commit", "-qam", "three"])?;
    let out = gs(
        dir,
        [
            "rebase", "main", "--empty", "keep", "--exec", command, "--json",
        ],
    )?;
    assert_eq!(out.status, Some(1), "{}\n{}", out.stdout, out.stderr);
    assert_eq!(parse_json(&out.stdout)?["result"], "conflicts");
    fs::write(dir.join("two"), "C\n")?;
    git(dir, &["add", "two"])?;
    let resume = gs(dir, ["rebase", "--continue", "--json"])?;
    assert!(resume.success(), "{}\n{}", resume.stdout, resume.stderr);
    assert_eq!(
        git(dir, &["log", "--format=%s", "main..feature"])?,
        "three\ntwo\none\n"
    );
    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn dirty_exec_human_and_markdown_explain_recovery() -> TestResult {
    for format in [None, Some("--markdown")] {
        let scratch = Scratch::new("dirty-exec-render")?;
        init_dirty_exec_fixture(scratch.path(), 1)?;
        let mut args = vec!["rebase", "main", "--exec", "echo edit >> f"];
        args.extend(format);
        let out = gs(scratch.path(), args)?;
        assert_eq!(out.status, Some(1));
        assert!(out.stdout.contains("preserved"));
        assert!(out.stdout.contains("grit rebase --continue"));
        assert!(out.stdout.contains("grit rebase --abort"));
        assert!(out.stdout.contains("f"));
    }
    Ok(())
}

#[test]
fn exec_recovery_with_grit_stash_resumes_without_repeating_dirty_command() -> TestResult {
    for history_alias in [false, true] {
        for skip in [false, true] {
            let scratch = Scratch::new("exec-recovery")?;
            let dir = scratch.path();
            let original_tip = init_dirty_exec_fixture(dir, 2)?;
            let mut prefix = vec![];
            if history_alias {
                prefix.push("history");
            }
            prefix.push("rebase");
            let mut args = prefix.clone();
            args.extend([
                "main",
                "--exec",
                "if ! test -f step-2; then echo hook-edit >> f; fi",
                "--exec",
                "echo resumed; echo next >> exec-log",
                "--json",
            ]);
            assert_eq!(gs(dir, args)?.status, Some(1));
            let stash = gs(dir, ["stash", "push", "-m", "exec edits", "--json"])?;
            assert!(stash.success(), "{}\n{}", stash.stdout, stash.stderr);
            let mut resume_args = prefix;
            resume_args.extend([if skip { "--skip" } else { "--continue" }, "--json"]);
            let resumed = gs(dir, resume_args)?;
            assert!(resumed.success(), "{}\n{}", resumed.stdout, resumed.stderr);
            let json = parse_json(&resumed.stdout)?;
            assert_eq!(json["result"], "completed");
            assert!(resumed.stderr.contains("resumed"));
            assert_ne!(git(dir, &["rev-parse", "feature"])?.trim(), original_tip);
            assert_eq!(
                git(dir, &["symbolic-ref", "HEAD"])?.trim(),
                "refs/heads/feature"
            );
            assert_eq!(
                fs::read_to_string(dir.join("exec-log"))?.lines().count(),
                if skip { 1 } else { 2 }
            );
            let pop = gs(dir, ["stash", "pop", "--json"])?;
            assert!(pop.success(), "{}\n{}", pop.stdout, pop.stderr);
            assert_eq!(fs::read_to_string(dir.join("f"))?, "base\nhook-edit\n");
            assert!(!dir.join(".git/rebase-merge").exists());
            git_fsck_strict(dir)?;
        }
    }
    Ok(())
}

#[test]
fn exec_failure_after_dirty_stop_forwards_streams_and_exit_code() -> TestResult {
    let scratch = Scratch::new("exec-resumed-failure")?;
    let dir = scratch.path();
    let tip = init_dirty_exec_fixture(dir, 1)?;
    let stopped = gs(
        dir,
        [
            "rebase",
            "main",
            "--exec",
            "echo edit >> f",
            "--exec",
            "echo captured; echo failed >&2; exit 7",
            "--json",
        ],
    )?;
    assert_eq!(stopped.status, Some(1));
    assert!(gs(dir, ["stash", "push"])?.success());
    let failed = gs(dir, ["rebase", "--continue", "--json"])?;
    assert_eq!(
        failed.status,
        Some(7),
        "{}\n{}",
        failed.stdout,
        failed.stderr
    );
    assert!(failed.stderr.contains("captured") && failed.stderr.contains("failed"));
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    assert!(!dir.join(".git/rebase-merge").exists());
    assert!(gs(dir, ["stash", "pop"])?.success());
    assert_eq!(fs::read_to_string(dir.join("f"))?, "base\nedit\n");
    git_fsck_strict(dir)?;
    Ok(())
}

/// Issue #2246: `--exec` must run when the branch is already based on the upstream.
#[test]
fn rebase_exec_runs_when_already_up_to_date_with_upstream() -> TestResult {
    let scratch = Scratch::new("issue-2246")?;
    let dir = scratch.path();
    git(dir, &["init", "-q", "-b", "main"])?;
    for i in 1..=3 {
        fs::write(dir.join(format!("f{i}.txt")), format!("{i}\n"))?;
        git(dir, &["add", "."])?;
        git(dir, &["commit", "-qm", &format!("c{i}")])?;
    }
    let tip = git(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    let upstream = git(dir, &["rev-parse", "HEAD~2"])?.trim().to_owned();

    let ok = gs(dir, ["rebase", "--exec", "true", &upstream, "--json"])?;
    assert!(ok.success(), "{}\n{}", ok.stdout, ok.stderr);
    let json = parse_json(&ok.stdout)?;
    assert_eq!(
        json.get("result").and_then(Value::as_str),
        Some("completed")
    );
    let exec = json
        .get("exec")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(exec, 2, "expected exec at c2 and c3: {json}");
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    assert!(
        !ok.stdout.contains("Already up to date"),
        "must not claim up to date when exec ran"
    );

    let failed = gs(dir, ["rebase", "--exec", "false", &upstream, "--json"])?;
    assert_eq!(
        failed.status,
        Some(1),
        "{}\n{}",
        failed.stdout,
        failed.stderr
    );
    assert_eq!(git(dir, &["rev-parse", "HEAD"])?.trim(), tip);
    git_fsck_strict(dir)?;
    Ok(())
}
