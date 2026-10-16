use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_maintenance_fsck_and_hook() -> Result<()> {
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

    let exe = env!("CARGO_BIN_EXE_guide_maintenance");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_maintenance failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("fsck_object_ok=1"));
    assert!(stdout.contains("pre_commit_ran=true"));

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .output()?;
    assert!(fsck.status.success());

    Ok(())
}
