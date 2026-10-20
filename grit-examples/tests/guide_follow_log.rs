use std::process::Command;

use anyhow::{Context, Result};

fn git(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .context("run git")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
fn guide_follow_log_matches_git() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path();
    git(repo, &["init", "-q", "-b", "main"])?;
    std::fs::write(repo.join("a.txt"), "v\n")?;
    git(repo, &["add", "a.txt"])?;
    git(repo, &["commit", "-q", "-m", "add"])?;
    git(repo, &["mv", "a.txt", "b.txt"])?;
    git(repo, &["commit", "-q", "-m", "rename"])?;

    let git_out = git(repo, &["log", "--follow", "--format=%H", "--", "b.txt"])?;
    let exe = env!("CARGO_BIN_EXE_guide_follow_log");
    let output = Command::new(exe)
        .args([repo.to_str().unwrap(), "b.txt"])
        .output()?;
    assert!(
        output.status.success(),
        "guide_follow_log failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let grit_lines: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.split_whitespace().next().unwrap_or("").to_owned())
        .collect();
    let git_lines: Vec<_> = git_out.lines().map(str::to_owned).collect();
    assert_eq!(grit_lines, git_lines);
    Ok(())
}
