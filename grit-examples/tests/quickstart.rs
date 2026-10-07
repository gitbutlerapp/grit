use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn quickstart_prints_head_matching_git() -> Result<()> {
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
    std::fs::write(repo.join("README.md"), "# Notes\n")?;
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-m", "Start the project"])
        .current_dir(repo)
        .output()?;

    let git_head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()?
            .stdout,
    )?
    .trim()
    .to_owned();

    let exe = env!("CARGO_BIN_EXE_quickstart");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "quickstart failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let printed = stdout
        .lines()
        .next()
        .context("quickstart printed nothing")?;
    assert_eq!(printed, git_head);
    assert!(stdout.contains("Start the project"));
    Ok(())
}
