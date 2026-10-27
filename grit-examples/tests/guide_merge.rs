use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn guide_merge_tree_matches_git_merge_tree() -> Result<()> {
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
    std::fs::write(repo.join("shared.txt"), b"base\n")?;
    Command::new("git")
        .args(["add", "shared.txt"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-qm", "base"])
        .current_dir(repo)
        .output()?;

    let base = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()?;
    assert!(base.status.success());
    let base_hex = String::from_utf8(base.stdout)?.trim().to_owned();

    Command::new("git")
        .args(["checkout", "-b", "ours"])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("ours-only.txt"), b"ours\n")?;
    Command::new("git")
        .args(["add", "ours-only.txt"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-qm", "ours"])
        .current_dir(repo)
        .output()?;

    Command::new("git")
        .args(["checkout", "-b", "theirs", &base_hex])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("theirs-only.txt"), b"theirs\n")?;
    Command::new("git")
        .args(["add", "theirs-only.txt"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-qm", "theirs"])
        .current_dir(repo)
        .output()?;

    let ours = Command::new("git")
        .args(["rev-parse", "ours"])
        .current_dir(repo)
        .output()?;
    let theirs = Command::new("git")
        .args(["rev-parse", "theirs"])
        .current_dir(repo)
        .output()?;
    let merge_base = Command::new("git")
        .args(["merge-base", "ours", "theirs"])
        .current_dir(repo)
        .output()?;
    assert!(merge_base.status.success());
    let merge_base_hex = String::from_utf8(merge_base.stdout)?.trim().to_owned();
    assert_eq!(merge_base_hex, base_hex);
    let ours_hex = String::from_utf8(ours.stdout)?.trim().to_owned();
    let theirs_hex = String::from_utf8(theirs.stdout)?.trim().to_owned();

    let git_tree = Command::new("git")
        .args([
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={merge_base_hex}"),
            &ours_hex,
            &theirs_hex,
        ])
        .current_dir(repo)
        .output()?;
    let git_merged = String::from_utf8(git_tree.stdout)?
        .lines()
        .next()
        .context("merge-tree tree line")?
        .trim()
        .to_owned();

    let exe = env!("CARGO_BIN_EXE_guide_merge");
    let output = Command::new(exe).current_dir(repo).output()?;
    assert!(
        output.status.success(),
        "guide_merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let grit_merged = stdout
        .lines()
        .find_map(|l| l.strip_prefix("merged_tree="))
        .context("merged_tree")?
        .trim()
        .to_owned();
    assert_eq!(grit_merged, git_merged);

    let merge_commit = stdout
        .lines()
        .find_map(|l| l.strip_prefix("merge_commit="))
        .context("merge_commit")?
        .trim()
        .to_owned();
    let tree_of_commit = Command::new("git")
        .args(["rev-parse", &format!("{merge_commit}^{{tree}}")])
        .current_dir(repo)
        .output()?;
    assert!(tree_of_commit.status.success());
    assert_eq!(String::from_utf8(tree_of_commit.stdout)?.trim(), git_merged);
    let fsck = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(repo)
        .output()?;
    assert!(
        fsck.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    Ok(())
}
