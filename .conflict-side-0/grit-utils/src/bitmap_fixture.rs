//! Cached `git.git` repository prepared for reachability bitmap benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use grit_lib::objects::bitmap::PackBitmapWriteOptions;
use grit_lib::prelude::RepositoryOptions;
use grit_lib::repo::Repository;

use crate::fixture::remove_dir_robust;
use crate::odb_fixture::odb_scratch_root;
use crate::real_repo_fixture::{ensure_git_git_bitmaps, run_git};
use crate::serve_request::ensure_serve_clone_request;

const GIT_GIT_URL: &str = "https://github.com/git/git.git";

pub use crate::real_repo_fixture::{
    assert_tag_pinned_fixture_refs, bitmap_isolated_env, list_branch_head_refs, DEFAULT_GIT_GIT_TAG,
};

/// Git-prepared bitmap fixture (`repack -adb`).
pub fn ensure_bitmaps_git_repo(git: &Path) -> Result<PathBuf> {
    ensure_git_git_bitmaps(git)
}

/// Path to the bitmap-prepared bare `git.git` clone.
pub fn bitmap_repo_cache_path() -> PathBuf {
    crate::real_repo_fixture::git_git_bitmaps_cache_path()
}

fn pinned_tag() -> String {
    std::env::var("GRIT_BENCH_GIT_GIT_TAG")
        .unwrap_or_else(|_| crate::real_repo_fixture::DEFAULT_GIT_GIT_TAG.to_owned())
}

const GRIT_READY_MARKER: &str = ".grit-bench-bitmaps-grit-ready";

/// Path to the bitmap-prepared bare `git.git` clone with **grit-written** pack bitmaps.
pub fn bitmap_grit_repo_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_BITMAPS_GRIT_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| odb_scratch_root().join("git.git-bitmaps-grit"))
}

fn stored_grit_fixture_tag(dest: &Path) -> Result<Option<String>> {
    let path = dest.join(GRIT_READY_MARKER);
    if !path.is_file() {
        return Ok(None);
    }
    let tag = fs::read_to_string(&path)
        .context("read grit bitmap ready marker")?
        .trim()
        .to_owned();
    Ok(Some(tag))
}

fn grit_fixture_is_current(dest: &Path, tag: &str) -> Result<bool> {
    let Some(stored) = stored_grit_fixture_tag(dest)? else {
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
    let pack_dir = dest.join("objects/pack");
    let has_bitmap = fs::read_dir(&pack_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|x| x == "bitmap"))
        })
        .unwrap_or(false);
    Ok(has_bitmap)
}

/// Bare `git.git` at a pinned tag, repacked without Git bitmaps, then `.bitmap` written by grit-lib.
pub fn ensure_bitmaps_grit_repo(git: &Path) -> Result<PathBuf> {
    let dest = bitmap_grit_repo_cache_path();
    let tag = pinned_tag();
    if grit_fixture_is_current(&dest, &tag)? {
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
        .ok_or_else(|| anyhow::anyhow!("invalid grit bitmap cache path {}", dest.display()))?;
    eprintln!(
        "Cloning {GIT_GIT_URL} at tag {tag} (bare, grit bitmaps) into {} …",
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
    eprintln!("Preparing grit bitmap fixture (repack -ad, commit-graph, PackBitmapWriter) …");
    run_git(git, &dest, &["repack", "-a", "-d"])?;
    run_git(git, &dest, &["commit-graph", "write", "--reachable"])?;
    let repo = Repository::open(&dest, &RepositoryOptions::empty())
        .context("open bare git.git for grit bitmap write")?;
    repo.write_largest_pack_bitmap(&PackBitmapWriteOptions::default(), SystemTime::UNIX_EPOCH)
        .context("write pack bitmap with grit")?;
    run_git(git, &dest, &["rev-list", "--test-bitmap", "HEAD"])
        .context("git rev-list --test-bitmap on grit-written bitmap")?;
    ensure_serve_clone_request(&dest)?;
    fs::write(dest.join(GRIT_READY_MARKER), tag.as_bytes()).context("write grit ready marker")?;
    Ok(dest)
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
