use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_stash_push_visible_to_git() -> Result<()> {
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
    std::fs::write(repo.join("f"), b"base\n")?;
    Command::new("git")
        .args(["add", "f"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-qm", "init"])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("f"), b"dirty\n")?;

    let exe = env!("CARGO_BIN_EXE_guide_stash");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_stash failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let list = Command::new("git")
        .args(["stash", "list"])
        .current_dir(repo)
        .output()?;
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout)?;
    assert!(
        list_text.contains("library guide stash") || list_text.contains("On main"),
        "git stash list:\n{list_text}"
    );

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .output()?;
    assert!(fsck.status.success());

    Ok(())
}
