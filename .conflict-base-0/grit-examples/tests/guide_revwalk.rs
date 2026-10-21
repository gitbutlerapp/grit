use std::process::Command;

use anyhow::{Context, Result};

fn git(dir: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .context("run git")?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[test]
fn guide_revwalk_matches_git_rev_list_and_merge_base() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path();
    git(repo, &["init", "-q", "-b", "main"])?;
    git(repo, &["commit", "-q", "--allow-empty", "-m", "root"])?;
    git(repo, &["branch", "feature"])?;
    git(repo, &["commit", "-q", "--allow-empty", "-m", "on-main"])?;
    git(repo, &["checkout", "-q", "feature"])?;
    git(repo, &["commit", "-q", "--allow-empty", "-m", "on-feature"])?;

    let exe = env!("CARGO_BIN_EXE_guide_revwalk");
    let output = Command::new(exe)
        .args([repo.to_str().unwrap(), "main..feature"])
        .output()?;
    assert!(
        output.status.success(),
        "guide_revwalk failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout)?;
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let merge_idx = lines
        .iter()
        .position(|l| l.starts_with("merge_base="))
        .context("merge_base line missing")?;
    let grit_commits: Vec<String> = lines[..merge_idx].iter().map(|s| (*s).to_owned()).collect();
    let grit_base = lines[merge_idx].trim_start_matches("merge_base=").trim();

    let git_commits: Vec<String> = git(repo, &["rev-list", "main..feature"])?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(grit_commits, git_commits, "rev-list output mismatch");

    let git_base = git(repo, &["merge-base", "main", "feature"])?;
    assert_eq!(grit_base, git_base.trim());

    Ok(())
}
