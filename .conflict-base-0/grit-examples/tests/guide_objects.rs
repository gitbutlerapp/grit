use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_objects_round_trip_with_system_git() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;

    let exe = env!("CARGO_BIN_EXE_guide_objects");
    let output = Command::new(exe).arg(repo).output()?;
    assert!(
        output.status.success(),
        "guide_objects failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout)?;
    let commit_oid = stdout
        .lines()
        .next()
        .context("guide_objects printed no commit oid")?
        .trim();

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .output()?;
    assert!(
        fsck.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    let cat = Command::new("git")
        .args(["cat-file", "-p", commit_oid])
        .current_dir(repo)
        .output()?;
    assert!(cat.status.success());
    let commit_text = String::from_utf8(cat.stdout)?;
    assert!(commit_text.contains("tree "));
    assert!(commit_text.contains("Library guide objects example"));

    Ok(())
}
