use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_index_tree_matches_write_tree() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path();

    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo)
        .output()
        .context("git init")?;
    std::fs::write(repo.join("tracked.txt"), b"hello\n")?;

    let exe = env!("CARGO_BIN_EXE_guide_index");
    let output = Command::new(exe).arg(repo).output()?;
    assert!(
        output.status.success(),
        "guide_index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let grit_tree = stdout
        .lines()
        .find_map(|l| l.strip_prefix("tree_oid="))
        .context("tree_oid line")?
        .trim();

    let git_tree = Command::new("git")
        .args(["write-tree"])
        .current_dir(repo)
        .output()?;
    assert!(git_tree.status.success());
    let git_tree_hex = String::from_utf8(git_tree.stdout)?.trim().to_owned();
    assert_eq!(git_tree_hex, grit_tree);

    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .output()?;
    assert!(
        fsck.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    Ok(())
}
