use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_notes_add_matches_git_notes_show() -> Result<()> {
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

    let exe = env!("CARGO_BIN_EXE_guide_notes");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_notes failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("library guide note"));

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()?;
    assert!(head.status.success());
    let head_hex = String::from_utf8(head.stdout)?.trim().to_owned();

    let show = Command::new("git")
        .args(["notes", "show", &head_hex])
        .current_dir(repo)
        .output()?;
    assert!(show.status.success());
    assert_eq!(String::from_utf8(show.stdout)?.trim(), "library guide note");

    Ok(())
}
