//! Cached `git.git` repository prepared for reachability bitmap benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench_env::{empty_global_config_path, isolated_env_prefix};
use crate::fixture::remove_dir_robust;
use crate::odb_fixture::odb_scratch_root;
use crate::serve_request::ensure_serve_clone_request;

const GIT_GIT_URL: &str = "https://github.com/git/git.git";

/// Default upstream tag for the bitmap fixture (override with `GRIT_BENCH_GIT_GIT_TAG`).
pub const DEFAULT_GIT_GIT_TAG: &str = "v2.47.0";

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn run_git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args(args).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Path to the bitmap-prepared bare `git.git` clone.
pub fn bitmap_repo_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_BITMAPS_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| odb_scratch_root().join("git.git-bitmaps"))
}

fn pinned_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

const READY_MARKER: &str = ".grit-bench-bitmaps-ready";

/// Ensure a bare clone of upstream `git.git` at a pinned tag, repacked with
/// pack bitmaps and a reachable commit-graph.
pub fn ensure_bitmaps_git_repo(git: &Path) -> Result<PathBuf> {
    let dest = bitmap_repo_cache_path();
    if dest.join(READY_MARKER).is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid bitmap cache path {}", dest.display()))?;
    let tag = pinned_tag();
    eprintln!(
        "Cloning {GIT_GIT_URL} at tag {tag} (bare) into {} …",
        dest.display()
    );
    run_git(
        git,
        parent,
        &["clone", "--bare", "-q", "--branch", &tag, GIT_GIT_URL, name],
    )?;
    eprintln!("Preparing bitmap fixture (repack -adb, commit-graph write --reachable) …");
    run_git(git, &dest, &["repack", "-a", "-d", "-b"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    ensure_serve_clone_request(&dest)?;
    fs::write(dest.join(READY_MARKER), tag.as_bytes()).context("write ready marker")?;
    Ok(dest)
}

/// Isolated git config prefix for bitmap scenarios (matches ODB benchmarks).
pub fn bitmap_isolated_env() -> String {
    isolated_env_prefix()
}
