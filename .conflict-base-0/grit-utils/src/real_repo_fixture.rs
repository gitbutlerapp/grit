//! Cached real-world and chromium-class benchmark repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench_env::empty_global_config_path;
use crate::fixture::remove_dir_robust;
use crate::odb_fixture::odb_scratch_root;
use crate::serve_request::ensure_serve_clone_request;

const GIT_GIT_URL: &str = "https://github.com/git/git.git";
const LINUX_URL: &str = "https://github.com/torvalds/linux.git";

/// Default upstream tag for git.git (override with `GRIT_BENCH_GIT_GIT_TAG`).
pub const DEFAULT_GIT_GIT_TAG: &str = "v2.47.0";

/// Default linux tag (override with `GRIT_BENCH_LINUX_TAG`).
pub const DEFAULT_LINUX_TAG: &str = "v6.12";

const BITMAP_READY: &str = ".grit-bench-bitmaps-ready";
const WORKTREE_READY: &str = ".grit-bench-worktree-ready";

/// Scratch root shared by real-repo fixtures (same as ODB cache unless overridden).
pub fn scratch_root() -> PathBuf {
    odb_scratch_root()
}

pub(crate) fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

pub(crate) fn run_git(git: &Path, dir: &Path, args: &[&str]) -> Result<()> {
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

fn pinned_git_git_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG").unwrap_or_else(|_| DEFAULT_GIT_GIT_TAG.to_owned())
}

fn pinned_linux_tag() -> String {
    std::env::var("GRIT_BENCH_LINUX_TAG").unwrap_or_else(|_| DEFAULT_LINUX_TAG.to_owned())
}

/// Whether linux fixtures are enabled (`--with-linux` or `GRIT_BENCH_LINUX=1`).
pub fn linux_fixture_enabled(cli_flag: bool) -> bool {
    cli_flag
        || std::env::var("GRIT_BENCH_LINUX")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Path to the bitmap-prepared bare `git.git` clone.
pub fn git_git_bitmaps_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_BITMAPS_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| scratch_root().join("git.git-bitmaps"))
}

fn stored_marker(dest: &Path, name: &str) -> Result<Option<String>> {
    let path = dest.join(name);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(
        fs::read_to_string(&path)
            .context("read fixture marker")?
            .trim()
            .to_owned(),
    ))
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
            "tag-pinned fixture must not contain branch refs after --single-branch tag clone; found: {}",
            heads.join(", ")
        );
    }
    Ok(())
}

fn git_git_bitmaps_current(dest: &Path, tag: &str) -> Result<bool> {
    let Some(stored) = stored_marker(dest, BITMAP_READY)? else {
        return Ok(false);
    };
    if stored != tag || !dest.join("HEAD").is_file() {
        return Ok(false);
    }
    Ok(assert_tag_pinned_fixture_refs(dest).is_ok())
}

/// Ensure a bare clone of upstream `git.git` at a pinned tag, repacked with pack bitmaps and commit-graph.
pub fn ensure_git_git_bitmaps(git: &Path) -> Result<PathBuf> {
    let dest = git_git_bitmaps_cache_path();
    let tag = pinned_git_git_tag();
    if git_git_bitmaps_current(&dest, &tag)? {
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
        .ok_or_else(|| anyhow::anyhow!("invalid git.git cache path {}", dest.display()))?;
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
    eprintln!("Preparing git.git fixture (repack -adb, commit-graph write --reachable) …");
    run_git(git, &dest, &["repack", "-a", "-d", "-b"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    ensure_serve_clone_request(&dest)?;
    fs::write(dest.join(BITMAP_READY), tag.as_bytes()).context("write bitmap ready marker")?;
    Ok(dest)
}

/// Path to cached linux bare clone.
pub fn linux_bare_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_LINUX_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| scratch_root().join("linux-bitmaps"))
}

const LINUX_READY: &str = ".grit-bench-linux-ready";

fn linux_current(dest: &Path, tag: &str) -> Result<bool> {
    let Some(stored) = stored_marker(dest, LINUX_READY)? else {
        return Ok(false);
    };
    Ok(stored == tag && dest.join("HEAD").is_file())
}

/// Ensure a bare linux kernel clone at a pinned tag, repacked with commit-graph.
pub fn ensure_linux_bare(git: &Path) -> Result<PathBuf> {
    let dest = linux_bare_cache_path();
    let tag = pinned_linux_tag();
    if linux_current(&dest, &tag)? {
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
        .ok_or_else(|| anyhow::anyhow!("invalid linux cache path {}", dest.display()))?;
    eprintln!(
        "Cloning {LINUX_URL} at tag {tag} (bare, single-branch) into {} …",
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
            LINUX_URL,
            name,
        ],
    )?;
    eprintln!("Preparing linux fixture (repack -adf, commit-graph write --reachable) …");
    run_git(git, &dest, &["repack", "-a", "-d", "-f"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    fs::write(dest.join(LINUX_READY), tag.as_bytes()).context("write linux ready marker")?;
    Ok(dest)
}

/// Cached worktree clone of a bare fixture for CLI scenarios.
pub fn ensure_worktree_clone(
    git: &Path,
    bare: &Path,
    worktree_name: &str,
    tag: &str,
) -> Result<PathBuf> {
    let dest = scratch_root().join(worktree_name);
    let marker = dest.join(WORKTREE_READY);
    if marker.is_file() {
        let stored = fs::read_to_string(&marker).context("read worktree marker")?;
        if stored.trim() == tag && dest.join(".git").exists() {
            return Ok(dest);
        }
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let bare_str = bare.to_str().context("bare path utf-8")?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("worktree name")?;
    eprintln!("Cloning {bare_str} into worktree {} …", dest.display());
    run_git(
        git,
        parent,
        &["clone", "-q", "--single-branch", bare_str, name],
    )?;
    run_git(git, &dest, &["checkout", "-B", "campaign-main"])?;
    fs::write(&marker, tag.as_bytes()).context("write worktree marker")?;
    Ok(dest)
}

/// Isolated git config prefix for bitmap scenarios (matches ODB benchmarks).
pub fn bitmap_isolated_env() -> String {
    crate::bench_env::isolated_env_prefix()
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
