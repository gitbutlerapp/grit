use std::process::Command;

use anyhow::{Context, Result};

fn parse_tree_diff(stdout: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_block = false;
    for line in stdout.lines() {
        if line == "tree_diff_begin" {
            in_block = true;
            continue;
        }
        if line == "tree_diff_end" {
            break;
        }
        if in_block {
            lines.push(line.to_owned());
        }
    }
    lines.sort();
    lines
}

#[test]
fn guide_diff_name_status_matches_git() -> Result<()> {
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
        .args(["config", "user.name", "Ada"])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("alpha.txt"), b"v1\n")?;
    Command::new("git")
        .args(["add", "alpha.txt"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-m", "first"])
        .current_dir(repo)
        .output()?;
    std::fs::write(repo.join("alpha.txt"), b"v2\n")?;
    Command::new("git")
        .args(["add", "alpha.txt"])
        .current_dir(repo)
        .output()?;
    Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(repo)
        .output()?;

    let exe = env!("CARGO_BIN_EXE_guide_diff");
    let output = Command::new(exe).arg(repo).output()?;
    assert!(
        output.status.success(),
        "guide_diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let grit_lines = parse_tree_diff(&stdout);

    let git_diff = Command::new("git")
        .args(["diff", "--name-status", "HEAD~1", "HEAD"])
        .current_dir(repo)
        .output()?;
    assert!(git_diff.status.success());
    let mut git_lines: Vec<String> = String::from_utf8(git_diff.stdout)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    git_lines.sort();

    assert_eq!(grit_lines, git_lines);

    Ok(())
}
