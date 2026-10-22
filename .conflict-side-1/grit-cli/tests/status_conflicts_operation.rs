//! Status JSON: typed conflicts and operation next_steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

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

fn gs_json(repo: &Path, args: &[&str]) -> Value {
    let mut cmd_args: Vec<&str> = args.to_vec();
    cmd_args.push("--json");
    let out = Command::new(GS)
        .args(&cmd_args)
        .current_dir(repo)
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

/// Exit code 2 means clap usage error (unknown flag/subcommand).
fn assert_grit_command_parses(cmd: &str) {
    let words = shell_words::split(cmd).unwrap_or_else(|e| panic!("split {cmd:?}: {e}"));
    assert_eq!(words.first().map(String::as_str), Some("grit"));
    let out = Command::new(GS)
        .args(&words[1..])
        .output()
        .unwrap_or_else(|e| panic!("run {cmd:?}: {e}"));
    assert_ne!(
        out.status.code(),
        Some(2),
        "usage error for {cmd:?}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn status_json_conflicts_and_operation_shape() -> TestResult {
    let tmp = tempfile::TempDir::new()?;
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f"), "base\n")?;
    system_git_ok(repo, &["add", "f"]);
    system_git_ok(repo, &["commit", "-qm", "base"]);
    system_git_ok(repo, &["checkout", "-qb", "side"]);
    fs::write(repo.join("f"), "side\n")?;
    system_git_ok(repo, &["commit", "-qam", "side"]);
    system_git_ok(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("f"), "main\n")?;
    system_git_ok(repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert_ne!(merge.status.code(), Some(0), "expected merge conflict");

    let v = gs_json(repo, &["status"]);
    assert_eq!(v["merging"], Value::Bool(true));
    assert_eq!(v["in_progress"], serde_json::json!(["merge"]));

    let conflicts = v["conflicts"].as_array().expect("conflicts array");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["path"], "f");
    assert_eq!(conflicts[0]["kind"], "both_modified");

    let op = v.get("operation").expect("operation object");
    assert_eq!(op["kind"], "merge");
    let steps = op["next_steps"].as_array().expect("next_steps");
    assert!(!steps.is_empty());
    for step in steps {
        let cmd = step.as_str().expect("step string");
        assert_grit_command_parses(cmd);
        assert!(
            !cmd.contains("commit"),
            "merge with conflicts must not suggest commit yet: {cmd}"
        );
    }

    Ok(())
}

#[test]
fn status_json_omits_operation_during_git_cherry_pick_conflict() -> TestResult {
    let tmp = tempfile::TempDir::new()?;
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    std::fs::write(repo.join("f"), "base\n")?;
    system_git_ok(repo, &["add", "f"]);
    system_git_ok(repo, &["commit", "-qm", "base"]);
    std::fs::write(repo.join("f"), "main\n")?;
    system_git_ok(repo, &["commit", "-qam", "main"]);
    system_git_ok(repo, &["checkout", "-q", "HEAD~1"]);
    std::fs::write(repo.join("f"), "other\n")?;
    system_git_ok(repo, &["commit", "-qam", "other"]);
    let pick = Command::new("git")
        .args(["cherry-pick", "main"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert_ne!(pick.status.code(), Some(0), "expected cherry-pick conflict");

    let v = gs_json(repo, &["status"]);
    assert!(v.get("operation").is_none() || v["operation"].is_null());
    assert!(!v["conflicts"].as_array().unwrap().is_empty());
    Ok(())
}

fn run_grit(repo: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(GS)
        .args(args)
        .current_dir(repo)
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_grit_shell(repo: &Path, cmd: &str) -> (i32, String, String) {
    let words = shell_words::split(cmd).expect("split");
    assert_eq!(words.first().map(String::as_str), Some("grit"));
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    run_grit(repo, &args)
}

#[test]
fn status_json_resolved_merge_next_steps_run_in_order() -> TestResult {
    let tmp = tempfile::TempDir::new()?;
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f"), "base\n")?;
    system_git_ok(repo, &["add", "f"]);
    system_git_ok(repo, &["commit", "-qm", "base"]);
    system_git_ok(repo, &["checkout", "-qb", "side"]);
    fs::write(repo.join("f"), "side\n")?;
    system_git_ok(repo, &["commit", "-qam", "side"]);
    system_git_ok(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("f"), "main\n")?;
    system_git_ok(repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert_ne!(merge.status.code(), Some(0), "expected merge conflict");
    fs::write(repo.join("f"), "resolved\n")?;
    system_git_ok(repo, &["add", "f"]);

    let v = gs_json(repo, &["status"]);
    let op = v.get("operation").expect("operation");
    let steps = op["next_steps"].as_array().expect("next_steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0], "grit merge --continue");
    let alts = op["alternatives"].as_array().expect("alternatives");
    assert_eq!(alts.len(), 1);
    assert_eq!(alts[0], "grit merge --abort");
    assert!(
        !steps
            .iter()
            .any(|s| s.as_str() == Some("grit merge --abort")),
        "abort must not appear in ordered next_steps"
    );

    for step in steps {
        let cmd = step.as_str().unwrap();
        assert_grit_command_parses(cmd);
        let (code, _, stderr) = run_grit_shell(repo, cmd);
        assert_eq!(code, 0, "ordered step {cmd} failed:\n{stderr}");
    }

    assert!(
        !repo.join(".git").join("MERGE_HEAD").exists(),
        "merge should finish after following next_steps"
    );
    Ok(())
}

fn init_merge_conflict_repo(repo: &Path) {
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f"), "base\n").expect("f");
    system_git_ok(repo, &["add", "f"]);
    system_git_ok(repo, &["commit", "-qm", "base"]);
    fs::write(repo.join("tick`name.txt"), "base\n").expect("tick path");
    system_git_ok(repo, &["add", "tick`name.txt"]);
    system_git_ok(repo, &["commit", "-qm", "tick"]);
    system_git_ok(repo, &["checkout", "-qb", "side"]);
    fs::write(repo.join("f"), "side\n").expect("f side");
    fs::write(repo.join("tick`name.txt"), "side\n").expect("tick side");
    system_git_ok(repo, &["commit", "-qam", "side"]);
    system_git_ok(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("f"), "main\n").expect("f main");
    fs::write(repo.join("tick`name.txt"), "main\n").expect("tick main");
    system_git_ok(repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("merge");
    assert_ne!(merge.status.code(), Some(0), "expected merge conflict");
}

#[test]
fn status_human_and_markdown_conflict_sections() -> TestResult {
    let tmp = tempfile::TempDir::new()?;
    let repo = tmp.path();
    init_merge_conflict_repo(repo);

    let (_, human, _) = run_grit(repo, &["status"]);
    assert!(
        human.contains("Conflicts\n  both modified (2)\n"),
        "human output should group unmerged paths by kind:\n{human}"
    );
    assert!(
        human.contains("    tick`name.txt\n\n→"),
        "human conflicts section should end with a blank line before hints:\n{human}"
    );

    let (_, md, _) = run_grit(repo, &["status", "--markdown"]);
    assert!(
        md.contains("## Conflicts\n"),
        "markdown conflicts use ## like other status sections:\n{md}"
    );
    assert!(
        !md.contains("### Conflicts"),
        "markdown must not use h3 for conflicts:\n{md}"
    );
    assert!(
        md.contains("  - ``tick`name.txt``"),
        "markdown paths must use format_inline_code (extra backticks around backtick paths):\n{md}"
    );
    assert!(
        md.contains("  - ``tick`name.txt``\n\n→"),
        "markdown conflicts section should end with a blank line before hints:\n{md}"
    );

    Ok(())
}
