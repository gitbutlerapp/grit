//! Cached fixtures for parallel scaling benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bench_env::empty_global_config_path;
use crate::fixture::{dirty_repo, remove_dir_robust, scratch_dir};
use crate::hot_path_fixture::{load_meta, setup_switch_fixture_in, HotPathMeta, HotPathRepoSpec};
use crate::odb_fixture::{ensure_hot_path_repacked, odb_scratch_root};

/// Production vs tiny smoke fixtures (mirrors network bench profiles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelProfile {
    Production,
    Smoke,
}

impl ParallelProfile {
    pub fn file_count(self) -> usize {
        match self {
            Self::Production => 100_000,
            Self::Smoke => 400,
        }
    }

    pub fn rename_count(self) -> usize {
        match self {
            Self::Production => 2000,
            Self::Smoke => 40,
        }
    }

    pub fn switch_spec(self) -> HotPathRepoSpec {
        match self {
            Self::Production => HotPathRepoSpec::heavy(),
            Self::Smoke => HotPathRepoSpec {
                file_count: 400,
                dir_count: 8,
                history_commits: 5,
                fsmonitor: false,
            },
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Self::Production => "100000",
            Self::Smoke => "smoke",
        }
    }
}

/// Root for cached parallel fixtures (`GRIT_BENCH_PARALLEL_CACHE`).
pub fn parallel_cache_root() -> PathBuf {
    std::env::var("GRIT_BENCH_PARALLEL_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/grit-bench-parallel-cache"))
}

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_global_config_path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "b@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "b@example.com");
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

fn rev_parse(git: &Path, dir: &Path, rev: &str) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", rev]).current_dir(dir);
    git_env(&mut cmd);
    let out = cmd.output().context("git rev-parse")?;
    if !out.status.success() {
        bail!("rev-parse {rev} failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn list_tracked_files_git(git: &Path, repo: &Path) -> Result<Vec<PathBuf>> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelBenchMeta {
    pub profile: String,
    pub default_branch: String,
    pub rename_old: String,
    pub rename_new: String,
    pub bare_path: String,
    pub pack_path: Option<String>,
}

pub fn meta_path(repo: &Path) -> PathBuf {
    repo.join(".grit-bench-parallel.json")
}

pub fn load_parallel_meta(repo: &Path) -> Result<ParallelBenchMeta> {
    let raw = fs::read_to_string(meta_path(repo)).context("read parallel meta")?;
    serde_json::from_str(&raw).context("parse parallel meta")
}

/// Worktree repo with `N` tracked files (cached).
pub fn ensure_worktree_repo(git: &Path, profile: ParallelProfile) -> Result<PathBuf> {
    match profile {
        ParallelProfile::Production => ensure_hot_path_repacked(git),
        ParallelProfile::Smoke => {
            let dest = parallel_cache_root().join(format!("worktree-{}", profile.suffix()));
            if meta_path(&dest).is_file() {
                return Ok(dest);
            }
            if dest.exists() {
                remove_dir_robust(&dest);
            }
            fs::create_dir_all(parallel_cache_root())?;
            create_repo_in(git, &dest, profile.file_count())?;
            write_meta(
                git,
                &dest,
                profile,
                None,
                rev_parse(git, &dest, "HEAD")?,
                rev_parse(git, &dest, "HEAD")?,
            )?;
            Ok(dest)
        }
    }
}

fn create_repo_in(git: &Path, dir: &Path, file_count: usize) -> Result<()> {
    fs::create_dir_all(dir)?;
    run_git(git, dir, &["init", "-q"])?;
    let files_per_dir = 100;
    let num_dirs = file_count.div_ceil(files_per_dir);
    let mut created = 0usize;
    for d in 0..num_dirs {
        let subdir = dir.join(format!("d{d:04}"));
        fs::create_dir_all(&subdir)?;
        for f in 0..files_per_dir {
            if created >= file_count {
                break;
            }
            fs::write(
                subdir.join(format!("f{f:04}.txt")),
                format!("content {d}/{f}\n"),
            )?;
            created += 1;
        }
    }
    run_git(git, dir, &["add", "-A"])?;
    run_git(git, dir, &["commit", "-q", "-m", "initial"])?;
    Ok(())
}

/// Bare clone of the worktree fixture for `file://` clone benchmarks.
pub fn ensure_bare_remote(git: &Path, profile: ParallelProfile) -> Result<PathBuf> {
    let worktree = ensure_worktree_repo(git, profile)?;
    let bare = parallel_cache_root().join(format!("bare-{}", profile.suffix()));
    if bare.join("HEAD").is_file() {
        return Ok(bare);
    }
    if bare.exists() {
        remove_dir_robust(&bare);
    }
    let parent = bare.parent().unwrap_or(Path::new("/tmp"));
    let name = bare
        .file_name()
        .and_then(|s| s.to_str())
        .context("bare path name")?;
    run_git(
        git,
        parent,
        &[
            "clone",
            "-q",
            "--bare",
            worktree.to_str().context("worktree utf8")?,
            name,
        ],
    )?;
    Ok(bare)
}

/// Bare remote whose objects are entirely in packfiles (repacked synthetic repo).
pub fn ensure_fully_packed_local_clone_bare(git: &Path, file_count: usize) -> Result<PathBuf> {
    let bare = parallel_cache_root().join(format!("local-clone-bare-repacked-{file_count}"));
    if bare.join("HEAD").is_file() {
        return Ok(bare);
    }
    let worktree = parallel_cache_root().join(format!("local-clone-wt-repacked-{file_count}"));
    if worktree.exists() {
        remove_dir_robust(&worktree);
    }
    create_repo_in(git, &worktree, file_count)?;
    run_git(git, &worktree, &["repack", "-a", "-d", "-f"])?;
    if bare.exists() {
        remove_dir_robust(&bare);
    }
    let parent = bare.parent().unwrap_or(Path::new("/tmp"));
    fs::create_dir_all(parent)?;
    let name = bare
        .file_name()
        .and_then(|s| s.to_str())
        .context("bare path name")?;
    run_git(
        git,
        parent,
        &[
            "clone",
            "-q",
            "--bare",
            worktree.to_str().context("worktree utf8")?,
            name,
        ],
    )?;
    Ok(bare)
}

/// Rename-heavy history: base commit and tip with ~`rename_count` renames.
pub fn ensure_rename_repo(
    git: &Path,
    profile: ParallelProfile,
) -> Result<(PathBuf, String, String)> {
    let dest = parallel_cache_root().join(format!("rename-{}", profile.suffix()));
    if meta_path(&dest).is_file() {
        let meta = load_parallel_meta(&dest)?;
        return Ok((dest, meta.rename_old, meta.rename_new));
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    match profile {
        ParallelProfile::Production => {
            let src = ensure_hot_path_repacked(git)?;
            clone_local_copy(git, &src, &dest)?;
        }
        ParallelProfile::Smoke => create_repo_in(git, &dest, profile.file_count())?,
    }
    let old_rev = rev_parse(git, &dest, "HEAD")?;
    let files = list_tracked_files_git(git, &dest)?;
    let n = profile.rename_count().min(files.len());
    for (i, rel) in files.iter().take(n).enumerate() {
        let from = dest.join(rel);
        let new_rel = PathBuf::from(format!("renamed/r{i:05}.txt"));
        let to = dest.join(&new_rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&from, &to)?;
    }
    run_git(git, &dest, &["add", "-A"])?;
    run_git(git, &dest, &["commit", "-q", "-m", "renames"])?;
    let new_rev = rev_parse(git, &dest, "HEAD")?;
    write_meta(git, &dest, profile, None, old_rev.clone(), new_rev.clone())?;
    Ok((dest, old_rev, new_rev))
}

fn default_branch(git: &Path, repo: &Path) -> Result<String> {
    let mut cmd = Command::new(git);
    cmd.args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(repo);
    git_env(&mut cmd);
    let out = cmd.output().context("git symbolic-ref")?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Ok("main".into())
}

fn write_meta(
    git: &Path,
    repo: &Path,
    profile: ParallelProfile,
    pack_path: Option<&Path>,
    rename_old: String,
    rename_new: String,
) -> Result<()> {
    let branch = default_branch(git, repo)?;
    let meta = ParallelBenchMeta {
        profile: profile.suffix().into(),
        default_branch: branch,
        rename_old,
        rename_new,
        bare_path: String::new(),
        pack_path: pack_path.map(|p| p.display().to_string()),
    };
    fs::write(meta_path(repo), serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

/// Wide switch fixture (10% path delta) cached under the parallel scratch root.
pub fn ensure_switch_wide_repo(
    git: &Path,
    profile: ParallelProfile,
) -> Result<(PathBuf, HotPathMeta)> {
    let dest = match profile {
        ParallelProfile::Production => odb_scratch_root().join("hot-path-100k-wide-switch"),
        ParallelProfile::Smoke => {
            parallel_cache_root().join(format!("switch-wide-{}", profile.suffix()))
        }
    };
    if crate::hot_path_fixture::meta_path(&dest).is_file() {
        return Ok((dest.clone(), load_meta(&dest)?));
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    if profile == ParallelProfile::Smoke {
        fs::create_dir_all(parallel_cache_root())?;
    } else {
        let parent = dest.parent().unwrap_or(Path::new("/tmp"));
        fs::create_dir_all(parent)?;
    }
    let spec = profile.switch_spec();
    let (_p, meta) = setup_switch_fixture_in(git, &dest, spec, true)?;
    Ok((dest, meta))
}

/// Repacked 100k repo and path to its primary `.pack` file.
pub fn ensure_repack_pack(git: &Path, profile: ParallelProfile) -> Result<(PathBuf, PathBuf)> {
    match profile {
        ParallelProfile::Production => {
            let repo = ensure_hot_path_repacked(git)?;
            let pack = find_primary_pack(&repo)?;
            Ok((repo, pack))
        }
        ParallelProfile::Smoke => {
            let repo = ensure_worktree_repo(git, profile)?;
            run_git(git, &repo, &["repack", "-a", "-d", "-f"])?;
            let pack = find_primary_pack(&repo)?;
            Ok((repo, pack))
        }
    }
}

fn find_primary_pack(repo: &Path) -> Result<PathBuf> {
    let git_dir = if repo.join(".git").is_dir() {
        repo.join(".git")
    } else {
        repo.to_path_buf()
    };
    let pack_dir = git_dir.join("objects/pack");
    let mut packs: Vec<PathBuf> = fs::read_dir(&pack_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    packs.sort();
    packs
        .into_iter()
        .next()
        .with_context(|| format!("no .pack under {}", pack_dir.display()))
}

/// Scratch directory for clone/checkout iterations (wiped by prepare hooks).
pub fn parallel_clone_dir() -> PathBuf {
    scratch_dir().join("parallel-clone")
}

/// Disposable clone used only to warm caches before fresh-checkout timings.
pub fn parallel_clone_warm_dir() -> PathBuf {
    scratch_dir().join("parallel-clone-warm")
}

/// Count worktree paths excluding `.git` (used to verify `clone --no-checkout`).
pub fn worktree_non_git_path_count(repo: &Path) -> Result<usize> {
    if !repo.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    count_worktree_paths(repo, &mut count)?;
    Ok(count)
}

#[allow(clippy::only_used_in_recursion)]
fn count_worktree_paths(dir: &Path, count: &mut usize) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            count_worktree_paths(&path, count)?;
        } else {
            *count += 1;
        }
    }
    Ok(())
}

/// Fail when the worktree is already materialized (fresh-checkout precondition).
pub fn assert_clone_no_checkout_worktree(repo: &Path) -> Result<()> {
    let n = worktree_non_git_path_count(repo)?;
    if n != 0 {
        bail!(
            "expected empty worktree after clone --no-checkout under {}, found {n} paths",
            repo.display()
        );
    }
    Ok(())
}

/// Copy of the pack file used for index-pack timing (idx removed each prepare).
pub fn index_pack_staging_dir() -> PathBuf {
    scratch_dir().join("parallel-index-pack")
}

/// Dirty copy of the synthetic repo, cached for stable status timings across suite runs.
pub fn prepare_status_dirty_repo(git: &Path, profile: ParallelProfile) -> Result<PathBuf> {
    let dest = parallel_cache_root().join(format!("status-dirty-{}", profile.suffix()));
    if meta_path(&dest).is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    fs::create_dir_all(parallel_cache_root())?;
    let src = ensure_worktree_repo(git, profile)?;
    clone_local_copy(git, &src, &dest)?;
    dirty_repo(&dest, profile.file_count())?;
    write_meta(
        git,
        &dest,
        profile,
        None,
        rev_parse(git, &dest, "HEAD")?,
        rev_parse(git, &dest, "HEAD")?,
    )?;
    Ok(dest)
}

pub fn prepare_status_clean_repo(git: &Path, profile: ParallelProfile) -> Result<PathBuf> {
    let dest = parallel_cache_root().join(format!("status-clean-{}", profile.suffix()));
    if meta_path(&dest).is_file() {
        return Ok(dest);
    }
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    fs::create_dir_all(parallel_cache_root())?;
    let src = ensure_worktree_repo(git, profile)?;
    clone_local_copy(git, &src, &dest)?;
    write_meta(
        git,
        &dest,
        profile,
        None,
        rev_parse(git, &dest, "HEAD")?,
        rev_parse(git, &dest, "HEAD")?,
    )?;
    Ok(dest)
}

fn clone_local_copy(git: &Path, src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        remove_dir_robust(dest);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new(git);
    cmd.args([
        "clone",
        "-q",
        "--local",
        src.to_str().context("src utf8")?,
        dest.to_str().context("dest utf8")?,
    ]);
    git_env(&mut cmd);
    let out = cmd.output().context("git clone --local")?;
    if !out.status.success() {
        bail!(
            "git clone --local failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn file_url(bare: &Path) -> Result<String> {
    let canonical = bare.canonicalize().context("canonicalize bare")?;
    Ok(format!("file://{}", canonical.display()))
}
