//! Worktree helpers for campaign scenarios on arbitrary repositories.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::real_repo_fixture::git_env;

/// List tracked files in a worktree (git ls-files).
pub fn list_tracked_files(git: &Path, repo: &Path) -> Result<Vec<PathBuf>> {
    let mut cmd = Command::new(git);
    cmd.args(["ls-files"]).current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git ls-files")?;
    if !out.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn is_safe_to_modify(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    if s == ".gitmodules" || s.starts_with(".git/") {
        return false;
    }
    true
}

/// Modify the first `count` tracked files (append a line) for dirty status / diff benches.
pub fn modify_tracked_files(repo: &Path, paths: &[PathBuf], count: usize) -> Result<()> {
    let safe: Vec<_> = paths.iter().filter(|p| is_safe_to_modify(p)).collect();
    let n = count.min(safe.len());
    anyhow::ensure!(n > 0, "need at least one tracked file to modify");
    for rel in safe.iter().take(n) {
        let path = repo.join(*rel);
        let mut file = fs::OpenOptions::new().append(true).open(&path)?;
        writeln!(file, "campaign bench line")?;
    }
    Ok(())
}

/// Dirty the worktree by modifying ~10% of tracked files and adding ~5% untracked files.
pub fn dirty_tracked_worktree(git: &Path, repo: &Path) -> Result<()> {
    let files = list_tracked_files(git, repo)?;
    let safe: Vec<_> = files
        .iter()
        .filter(|p| is_safe_to_modify(p))
        .cloned()
        .collect();
    let modify_count = (safe.len() / 10).max(1);
    modify_tracked_files(repo, &safe, modify_count)?;
    let untracked = (files.len() / 20).max(1);
    for i in 0..untracked {
        fs::write(
            repo.join(format!(".grit-campaign-untracked-{i}.txt")),
            format!("untracked {i}\n"),
        )?;
    }
    Ok(())
}

/// Pick a stable blame target: prefer small root files, else the shortest tracked path.
pub fn blame_target_path(git: &Path, repo: &Path) -> Result<String> {
    for candidate in ["README.md", "version", "COPYING", "Makefile"] {
        if repo.join(candidate).is_file() {
            return Ok(candidate.to_string());
        }
    }
    let files = list_tracked_files(git, repo)?;
    files
        .into_iter()
        .min_by_key(|p| p.as_os_str().len())
        .map(|p| p.display().to_string())
        .ok_or_else(|| anyhow::anyhow!("no tracked files for blame"))
}
