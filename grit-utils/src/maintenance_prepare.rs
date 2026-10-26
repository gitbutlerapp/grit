//! Hyperfine `--prepare` hook: restore the maintenance benchmark worktree.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::maintenance_suite::{git_env, seed_loose_objects};
use crate::real_repo_fixture::run_git;

const BARE_MARKER: &str = "maintenance-bare.path";

/// Path written by [`crate::maintenance_suite::run_maintenance_suite`] for prepare resets.
pub fn maintenance_bare_marker_path() -> PathBuf {
    scratch_dir().join(BARE_MARKER)
}

/// Re-clone the deep-history bare fixture and re-seed loose objects before each benchmark run.
pub fn run_prepare_maintenance(git: &Path) -> Result<()> {
    if let Ok(campaign_cwd) = std::env::var("GRIT_BENCH_CAMPAIGN_CWD") {
        let repo = PathBuf::from(campaign_cwd);
        if let Ok(meta) = crate::campaign_prep::load_campaign_hot_path_meta(&repo) {
            crate::campaign_prep::restore_campaign_worktree_to_main(git, &repo, &meta)?;
        } else {
            run_git(git, &repo, &["reset", "--hard", "HEAD"])?;
            run_git(
                git,
                &repo,
                &[
                    "clean",
                    "-fd",
                    "-q",
                    "-e",
                    ".grit-bench-worktree-ready",
                    "-e",
                    ".grit-bench-hot-path.json",
                ],
            )?;
        }
        return seed_loose_objects(git, &repo);
    }

    let bare = std::fs::read_to_string(maintenance_bare_marker_path())
        .context("read maintenance bare marker (run maintenance suite first)")?;
    let bare = PathBuf::from(bare.trim());
    let worktree = scratch_dir().join("gc-medium-worktree");
    if worktree.exists() {
        if let Some(parent) = worktree.parent() {
            let _ = std::env::set_current_dir(parent);
        }
    }
    remove_dir_robust(&worktree);
    let mut cmd = Command::new(git);
    cmd.args([
        "clone",
        bare.to_str().context("bare path utf-8")?,
        worktree.to_str().context("worktree path utf-8")?,
    ]);
    git_env(&mut cmd);
    let out = cmd.output().context("git clone for prepare")?;
    if !out.status.success() {
        anyhow::bail!(
            "prepare clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    seed_loose_objects(git, &worktree)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn campaign_prepare_maintenance_uses_campaign_cwd() -> Result<()> {
        let git = PathBuf::from("git");
        let dir = tempfile::tempdir()?;
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo)?;
        let init = Command::new(&git)
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .output()?;
        assert!(init.status.success());
        std::fs::write(repo.join("f.txt"), b"x")?;
        let add = Command::new(&git)
            .args(["add", "f.txt"])
            .current_dir(&repo)
            .output()?;
        assert!(add.status.success());
        let commit = Command::new(&git)
            .args(["commit", "-qm", "init"])
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .current_dir(&repo)
            .output()?;
        assert!(commit.status.success());

        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", &repo);
        run_prepare_maintenance(&git)?;
        std::env::remove_var("GRIT_BENCH_CAMPAIGN_CWD");
        Ok(())
    }
}
