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

fn stored_fixture_tag(dest: &Path) -> Result<Option<String>> {
    let path = dest.join(READY_MARKER);
    if !path.is_file() {
        return Ok(None);
    }
    let tag = fs::read_to_string(&path)
        .context("read bitmap ready marker")?
        .trim()
        .to_owned();
    Ok(Some(tag))
}

/// Branch refs that must not appear after a single-branch tag clone.
pub fn list_branch_head_refs(repo: &Path) -> Result<Vec<String>> {
    let mut refs = grit_lib::refs::list_refs(repo, "refs/heads/")?;
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(refs.into_iter().map(|(name, _)| name).collect())
}

/// A tag-only bitmap fixture must not retain upstream branch tips.
pub fn assert_tag_pinned_fixture_refs(repo: &Path) -> Result<()> {
    let heads = list_branch_head_refs(repo)?;
    if !heads.is_empty() {
        bail!(
            "bitmap fixture must not contain branch refs after --single-branch tag clone; found: {}",
            heads.join(", ")
        );
    }
    Ok(())
}

fn fixture_is_current(dest: &Path, tag: &str) -> Result<bool> {
    let Some(stored) = stored_fixture_tag(dest)? else {
        return Ok(false);
    };
    if stored != tag {
        return Ok(false);
    }
    if !dest.join("HEAD").is_file() {
        return Ok(false);
    }
    if assert_tag_pinned_fixture_refs(dest).is_err() {
        return Ok(false);
    }
    Ok(true)
}

/// Ensure a bare clone of upstream `git.git` at a pinned tag, repacked with
/// pack bitmaps and a reachable commit-graph.
pub fn ensure_bitmaps_git_repo(git: &Path) -> Result<PathBuf> {
    let dest = bitmap_repo_cache_path();
    let tag = pinned_tag();
    if fixture_is_current(&dest, &tag)? {
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
    eprintln!(
        "Cloning {GIT_GIT_URL} at tag {tag} (bare, single-branch) into {} …",
        dest.display()
    );
    run_git(
        git,
        parent,
        &[
            "clone",
            "--bare",
            "-q",
            "--single-branch",
            "--branch",
            &tag,
            GIT_GIT_URL,
            name,
        ],
    )?;
    assert_tag_pinned_fixture_refs(&dest)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_init_bare_with_branches(dir: &Path) -> Result<()> {
        Command::new("git")
            .args(["init", "--bare", "-b", "master"])
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .context("git init bare")?
            .success()
            .then_some(())
            .ok_or_else(|| anyhow::anyhow!("git init bare failed"))?;
        Ok(())
    }

    #[test]
    fn single_branch_tag_clone_drops_other_branch_refs() -> Result<()> {
        let origin = TempDir::new()?;
        git_init_bare_with_branches(origin.path())?;
        let source = TempDir::new()?;
        Command::new("git")
            .args(["clone", origin.path().to_str().unwrap(), "work"])
            .current_dir(source.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        let work = source.path().join("work");
        fs::write(work.join("file.txt"), b"v1")?;
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["commit", "-m", "one"])
            .current_dir(&work)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()?;
        Command::new("git")
            .args(["tag", "v1.0"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["branch", "moving"])
            .current_dir(&work)
            .status()?;
        Command::new("git")
            .args(["push", "origin", "master", "moving", "v1.0"])
            .current_dir(&work)
            .status()?;

        let clone_parent = TempDir::new()?;
        let clone_path = clone_parent.path().join("dest.git");
        Command::new("git")
            .args([
                "clone",
                "--bare",
                "--single-branch",
                "--branch",
                "v1.0",
                origin.path().to_str().unwrap(),
                clone_path.to_str().unwrap(),
            ])
            .current_dir(clone_parent.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()?;
        assert_tag_pinned_fixture_refs(&clone_path)?;
        let heads = list_branch_head_refs(&clone_path)?;
        assert!(heads.is_empty(), "unexpected heads: {heads:?}");
        Ok(())
    }
}
