use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_repository_reports_git_dir_and_config() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;
    Command::new("git")
        .args(["config", "user.email", "ada@example.com"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["config", "user.name", "Ada Lovelace"])
        .current_dir(repo)
        .output()?;

    let exe = env!("CARGO_BIN_EXE_guide_repository");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_repository failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("git_dir="));
    assert!(stdout.contains("work_tree="));
    assert!(stdout.contains("user.name=Ada Lovelace"));
    Ok(())
}
