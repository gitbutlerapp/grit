//! Agent-style workflow: path-scoped status/add/commit with dry-run, then merge conflict resolution.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static OsStr {
    if cfg!(windows) {
        OsStr::new("NUL")
    } else {
        OsStr::new("/dev/null")
    }
}

fn system_git_ok(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_porcelain_v2(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["status", "--porcelain=v2", "-b"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git status");
    assert!(out.status.success(), "git status --porcelain=v2");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_fsck_strict(repo: &Path) {
    system_git_ok(repo, &["fsck", "--strict"]);
}

fn git_log_oneline(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["log", "--oneline", "-5"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git log");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit_json(repo: &Path, args: &[&str]) -> Value {
    let mut cmd_args: Vec<&str> = Vec::new();
    if let Some(i) = args.iter().position(|a| *a == "--") {
        cmd_args.extend_from_slice(&args[..i]);
        cmd_args.push("--json");
        cmd_args.extend_from_slice(&args[i..]);
    } else {
        cmd_args.extend_from_slice(args);
        cmd_args.push("--json");
    }
    let out = Command::new(GRIT)
        .args(&cmd_args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.com")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "json parse ({e}): stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn grit_run(repo: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.com")
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn grit_commit_stdin(repo: &Path, args: &[&str], message: &str) -> (i32, String, String) {
    let mut child = Command::new(GRIT)
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("GIT_AUTHOR_NAME", "Agent")
        .env("GIT_AUTHOR_EMAIL", "agent@example.com")
        .env("GIT_COMMITTER_NAME", "Agent")
        .env("GIT_COMMITTER_EMAIL", "agent@example.com")
        .spawn()
        .expect("spawn grit");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(message.as_bytes())
        .expect("write message");
    let out = child.wait_with_output().expect("wait grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn assert_git_clean_enough(repo: &Path, step: &str) {
    git_fsck_strict(repo);
    let porcelain = git_porcelain_v2(repo);
    assert!(
        !porcelain.contains("u "), // no unmerged
        "{step}: git saw unmerged entries:\n{porcelain}"
    );
}

#[test]
fn precise_commit_workflow_with_upstream_and_merge_conflict() -> TestResult {
    let tmp = tempfile::TempDir::new()?;
    let remote = tmp.path().join("remote.git");
    let clone = tmp.path().join("clone");
    fs::create_dir_all(tmp.path())?;
    system_git_ok(
        tmp.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );

    system_git_ok(tmp.path(), &["init", "-q", "-b", "main", "seed"]);
    let seed = tmp.path().join("seed");
    system_git_ok(&seed, &["config", "user.name", "Agent"]);
    system_git_ok(&seed, &["config", "user.email", "agent@example.com"]);
    fs::create_dir_all(seed.join("src"))?;
    fs::write(seed.join("src/base.txt"), "base\n")?;
    fs::write(seed.join("src/other.txt"), "other\n")?;
    system_git_ok(&seed, &["add", "."]);
    system_git_ok(&seed, &["commit", "-qm", "seed"]);
    system_git_ok(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    system_git_ok(&seed, &["push", "-u", "origin", "main"]);

    system_git_ok(
        tmp.path(),
        &[
            "clone",
            "-q",
            "-b",
            "main",
            remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    system_git_ok(&clone, &["config", "branch.main.remote", "origin"]);
    system_git_ok(&clone, &["config", "branch.main.merge", "refs/heads/main"]);

    // Agent edits under src/
    fs::write(clone.join("src/base.txt"), "edited base\n")?;
    fs::write(clone.join("src/new.txt"), "new file\n")?;
    fs::write(clone.join("outside.txt"), "leave unstaged\n")?;

    let status = grit_json(&clone, &["status", "--", "src"]);
    assert!(!status["clean"].as_bool().unwrap_or(true));
    assert_git_clean_enough(&clone, "after status");

    let add_preview = grit_json(&clone, &["add", "--dry-run", "-u", "--", "src"]);
    assert_eq!(add_preview["dry_run"], Value::Bool(true));

    let path_preview = grit_json(
        &clone,
        &["commit", "--dry-run", "-m", "preview", "--", "src/new.txt"],
    );
    assert_eq!(path_preview["dry_run"], Value::Bool(true));
    assert_eq!(path_preview["scope"], "paths");
    let preview_files = path_preview["files"].as_array().expect("preview files");
    assert!(
        preview_files.iter().any(|f| f["path"] == "src/new.txt"),
        "path dry-run should list new file: {preview_files:?}"
    );

    let (code, _, stderr) = grit_run(&clone, &["add", "-u", "--", "src/base.txt"]);
    assert_eq!(code, 0, "add -u base failed: {stderr}");
    assert_git_clean_enough(&clone, "after add -u base");

    let real_new = grit_json(
        &clone,
        &["commit", "-m", "Add new under src", "--", "src/new.txt"],
    );
    assert_eq!(real_new["scope"], "paths");
    assert_eq!(real_new["changes"], 1);
    assert_git_clean_enough(&clone, "after path commit new");

    fs::write(clone.join("src/to_drop.txt"), "drop me\n")?;
    system_git_ok(&clone, &["add", "src/to_drop.txt"]);
    system_git_ok(&clone, &["commit", "-qm", "track drop"]);
    fs::remove_file(clone.join("src/to_drop.txt"))?;
    let del_preview = grit_json(
        &clone,
        &[
            "commit",
            "--dry-run",
            "-m",
            "drop preview",
            "--",
            "src/to_drop.txt",
        ],
    );
    assert_eq!(del_preview["files"].as_array().unwrap().len(), 1);
    let del_real = grit_json(
        &clone,
        &["commit", "-m", "drop file", "--", "src/to_drop.txt"],
    );
    assert_eq!(del_real["changes"], 1);
    assert_git_clean_enough(&clone, "after path commit delete");

    fs::write(clone.join("src/base.txt"), "edited base again\n")?;
    let (code, _, stderr) = grit_run(&clone, &["add", "-u", "--", "src/base.txt"]);
    assert_eq!(code, 0, "re-stage base: {stderr}");
    let chmod_preview = grit_json(
        &clone,
        &[
            "commit",
            "--dry-run",
            "-m",
            "mode preview",
            "--",
            "src/base.txt",
        ],
    );
    assert_eq!(chmod_preview["files"].as_array().unwrap().len(), 1);

    let (code, stdout, stderr) = grit_commit_stdin(
        &clone,
        &["commit", "--staged", "-F", "-", "--json"],
        "Record base only\n",
    );
    assert_eq!(code, 0, "commit --staged -F - failed: {stderr}\n{stdout}");
    let commit_v: Value = serde_json::from_str(&stdout)?;
    assert_eq!(commit_v["scope"], "staged");
    assert_git_clean_enough(&clone, "after partial commit");
    let log = git_log_oneline(&clone);
    assert!(log.contains("Record base only"));

    // Merge conflict workflow
    system_git_ok(&clone, &["checkout", "-qb", "side"]);
    fs::write(clone.join("src/base.txt"), "side line\n")?;
    system_git_ok(&clone, &["commit", "-qam", "side"]);
    system_git_ok(&clone, &["checkout", "main"]);
    fs::write(clone.join("src/base.txt"), "main line\n")?;
    system_git_ok(&clone, &["add", "src/base.txt"]);
    system_git_ok(&clone, &["commit", "-qm", "main tip"]);

    let merge = grit_run(&clone, &["merge", "side", "--json"]);
    assert_ne!(merge.0, 0, "expected merge conflict");

    let conflict_status = grit_json(&clone, &["status"]);
    let conflicts = conflict_status["conflicts"]
        .as_array()
        .expect("conflicts array");
    assert!(!conflicts.is_empty());
    let op = conflict_status
        .get("operation")
        .expect("operation during merge");
    let steps = op["next_steps"].as_array().expect("next_steps");
    assert!(!steps.is_empty());
    git_fsck_strict(&clone);

    fs::write(clone.join("src/base.txt"), "resolved\n")?;
    system_git_ok(&clone, &["add", "src/base.txt"]);
    let (c, _, e) = grit_run(&clone, &["commit", "-m", "Merge side", "--json"]);
    assert_eq!(c, 0, "merge commit failed: {e}");
    assert_git_clean_enough(&clone, "after merge commit");
    assert!(
        !clone.join(".git").join("MERGE_HEAD").exists(),
        "merge should be complete"
    );
    let final_log = git_log_oneline(&clone);
    assert!(
        final_log.lines().count() >= 3,
        "expected merge commit on history:\n{final_log}"
    );
    Ok(())
}
