//! Hyperfine `--prepare` hook: restore the maintenance benchmark worktree.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::maintenance_suite::{git_env, seed_loose_objects};

const BARE_MARKER: &str = "maintenance-bare.path";

/// Path written by [`crate::maintenance_suite::run_maintenance_suite`] for prepare resets.
pub fn maintenance_bare_marker_path() -> PathBuf {
    scratch_dir().join(BARE_MARKER)
}

/// Re-clone the deep-history bare fixture and re-seed loose objects before each benchmark run.
pub fn run_prepare_maintenance(git: &Path) -> Result<()> {
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
