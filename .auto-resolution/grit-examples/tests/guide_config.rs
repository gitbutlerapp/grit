use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_config_set_local_matches_git() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;

    let exe = env!("CARGO_BIN_EXE_guide_config");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("user.name=Library Guide Config"));

    let git_name = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(repo)
        .output()?;
    assert!(git_name.status.success());
    assert_eq!(
        String::from_utf8(git_name.stdout)?.trim(),
        "Library Guide Config"
    );

    Ok(())
}
