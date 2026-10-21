//! `grit clone` when the remote HEAD is unborn but other branches exist (issue #1069).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?;
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        dir.display()
    );
    Ok(())
}

fn grit_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, Box<dyn Error>> {
    Ok(Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?)
}

#[test]
fn clone_unborn_remote_head_with_main_branch_does_not_claim_empty() -> TestResult {
    let base = tempfile::tempdir()?;
    let remote = base.path().join("remote.git");
    git_in(
        base.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    )?;

    let src = base.path().join("src");
    fs::create_dir_all(&src)?;
    grit_in(&src, &["init"])?;
    grit_in(&src, &["config", "user.name", "a"])?;
    grit_in(&src, &["config", "user.email", "a@b.c"])?;
    fs::write(src.join("f"), b"x\n")?;
    grit_in(&src, &["add", "f"])?;
    grit_in(&src, &["commit", "-m", "c1"])?;
    grit_in(&src, &["remote", "add", "origin", remote.to_str().unwrap()])?;
    let push = grit_in(&src, &["push"])?;
    assert!(
        push.status.success(),
        "grit push: {}",
        String::from_utf8_lossy(&push.stderr)
    );

    let dest = base.path().join("clone2");
    let out = grit_in(base.path(), &["clone", remote.to_str().unwrap(), "clone2"])?;
    assert!(
        out.status.success(),
        "grit clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.to_ascii_lowercase().contains("empty"),
        "stderr should not say the remote is empty: {stderr}"
    );
    assert!(
        stderr.contains("no commits yet") || stderr.contains("no files were checked out"),
        "expected unborn-default warning: {stderr}"
    );

    let branches = Command::new("git")
        .args(["branch", "-a"])
        .current_dir(&dest)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(branches.status.success());
    let branch_text = String::from_utf8_lossy(&branches.stdout);
    assert!(
        branch_text.contains("origin/main"),
        "expected origin/main tracking ref: {branch_text}"
    );

    let json = grit_in(
        base.path(),
        &["clone", "--json", remote.to_str().unwrap(), "clone3"],
    )?;
    assert!(json.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(parsed["no_checkout_reason"], "unborn_default_branch");
    assert_eq!(parsed["files"], 0);
    assert!(parsed["objects"].as_u64().unwrap_or(0) > 0);
    let tips = parsed["branches_with_commits"].as_array().expect("tips");
    assert!(tips.iter().any(|v| v == "main"), "{parsed}");

    Ok(())
}
