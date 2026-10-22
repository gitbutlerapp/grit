//! Cached repositories for ODB read benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::bench_env::{empty_global_config_path, isolated_env_prefix};
use crate::fixture::remove_dir_robust;
use crate::hot_path_fixture::{setup_switch_fixture_in, HotPathRepoSpec};

const GIT_GIT_URL: &str = "https://github.com/git/git.git";

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

/// Root directory for cached benchmark clones (override with `GRIT_BENCH_ODB_CACHE`).
pub fn odb_scratch_root() -> PathBuf {
    std::env::var("GRIT_BENCH_ODB_CACHE")
        .or_else(|_| std::env::var("GRIT_BENCH_SCRATCH"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-odb-cache"))
}

/// Path to cached bare `git.git` (override with `GRIT_BENCH_GIT_GIT`).
pub fn git_git_cache_path() -> PathBuf {
    std::env::var("GRIT_BENCH_GIT_GIT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| odb_scratch_root().join("git.git"))
}

/// Ensure a bare clone of upstream `git.git` exists and return its path.
pub fn ensure_git_git_bare(git: &Path) -> Result<PathBuf> {
    let dest = git_git_cache_path();
    if dest.join("HEAD").is_file() {
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
    eprintln!("Cloning {GIT_GIT_URL} (bare) into {} …", dest.display());
    run_git(git, parent, &["clone", "--bare", "-q", GIT_GIT_URL, name])?;
    Ok(dest)
}

/// Large synthetic repo (100k files, 1000 commits) repacked with `git repack -adf`.
pub fn ensure_hot_path_repacked(git: &Path) -> Result<PathBuf> {
    let dest = odb_scratch_root().join("hot-path-100k-repacked");
    if dest.join(".grit-bench-odb-ready").is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let spec = HotPathRepoSpec::heavy();
    setup_switch_fixture_in(git, &dest, spec, false)?;
    run_git(git, &dest, &["repack", "-a", "-d", "-f"])?;
    fs::write(dest.join(".grit-bench-odb-ready"), b"ok")?;
    Ok(dest)
}

/// Sorted object-id list for `cat-file --batch` (shared by git and grit drivers).
pub fn write_sorted_oid_list(git: &Path, repo: &Path, dest: &Path) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args([
        "cat-file",
        "--batch-all-objects",
        "--unordered",
        "--batch-check=%(objectname)",
    ])
    .current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git cat-file batch-check oids")?;
    if !out.status.success() {
        bail!(
            "git cat-file batch-check failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    lines.sort();
    let body = lines.join("\n");
    fs::write(dest, format!("{body}\n")).context("write sorted oid list")?;
    Ok(())
}

pub fn sorted_oid_list_path(repo: &Path) -> PathBuf {
    repo.join(".grit-bench-sorted-oids.txt")
}

pub fn ensure_sorted_oid_list(git: &Path, repo: &Path) -> Result<PathBuf> {
    let path = sorted_oid_list_path(repo);
    if !path.is_file() {
        write_sorted_oid_list(git, repo, &path)?;
    }
    Ok(path)
}

/// Environment prefix for hermetic git in ODB scenarios.
pub fn odb_isolated_env() -> String {
    isolated_env_prefix()
}
