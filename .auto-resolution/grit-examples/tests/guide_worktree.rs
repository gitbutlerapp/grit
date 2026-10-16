use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_worktree_list_matches_git() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;
    Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("f"), b"x\n")?;
    Command::new("git")
        .args(["add", "f"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(repo)
        .output()?;

    let linked = tempfile::tempdir_in(repo.parent().unwrap())?;
    Command::new("git")
        .args([
            "worktree",
            "add",
            linked.path().to_str().unwrap(),
            "-b",
            "linked",
        ])
        .current_dir(repo)
        .output()
        .context("git worktree add")?;

    let exe = env!("CARGO_BIN_EXE_guide_worktree");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_worktree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let grit_count = stdout
        .lines()
        .find_map(|l| l.strip_prefix("worktree_count="))
        .context("worktree_count")?
        .trim()
        .parse::<usize>()?;

    let git_list = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo)
        .output()?;
    assert!(git_list.status.success());
    let git_count = String::from_utf8(git_list.stdout)?
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count();
    assert_eq!(grit_count, git_count);

    Ok(())
}
