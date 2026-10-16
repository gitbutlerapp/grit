//! Worktree benchmark fixtures from the registry cache (synth tiers + git.git).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::campaign_worktree::{list_tracked_files, modify_tracked_files};
use crate::fixture::{bench_work_dir, remove_dir_robust, scratch_dir};
use crate::fixture_registry::{self, FixtureKind};
use crate::real_repo_fixture::run_git;
use crate::schema::FixtureRecord;
use crate::shell::shell_quote;

const PATHS_MARKER: &str = ".grit-bench-worktree-paths";
const MODIFY_COUNT_MARKER: &str = ".grit-bench-worktree-modify-count";
const ACTIVE_REPO_MARKER: &str = ".grit-bench-worktree-active";

/// Fraction of tracked files to touch in mutating scenarios (~1%).
pub const MODIFY_FRACTION_DENOM: usize = 100;

/// Return the sorted path list used for ~1% modifications (relative paths).
pub fn modified_path_list(git: &Path, repo: &Path) -> Result<Vec<PathBuf>> {
    let count = read_modify_count(repo)?;
    let mut rel = list_tracked_files(git, repo)?
        .into_iter()
        .filter(|p| p.to_string_lossy() != ".gitmodules")
        .collect::<Vec<_>>();
    rel.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    Ok(rel.into_iter().take(count).collect())
}

fn meta_paths() -> (PathBuf, PathBuf) {
    let root = scratch_dir();
    (root.join(MODIFY_COUNT_MARKER), root.join(PATHS_MARKER))
}

fn read_modify_count(_repo: &Path) -> Result<usize> {
    let (count_path, _) = meta_paths();
    let raw = fs::read_to_string(&count_path).context("read modify count")?;
    raw.trim()
        .parse::<usize>()
        .context("parse modify count marker")
}

fn write_modify_count(_repo: &Path, count: usize) -> Result<()> {
    let (count_path, _) = meta_paths();
    fs::write(&count_path, format!("{count}\n")).context("write modify count marker")
}

fn write_paths_marker(_repo: &Path, paths: &[PathBuf]) -> Result<()> {
    let (_, paths_path) = meta_paths();
    let body = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&paths_path, format!("{body}\n")).context("write paths marker")
}

/// Pin config knobs that would otherwise differ between git and grit runs.
pub fn pin_worktree_bench_config(git: &Path, repo: &Path) -> Result<()> {
    for (key, value) in [
        ("core.untrackedCache", "false"),
        ("core.fsmonitor", "false"),
    ] {
        run_git(git, repo, &["config", key, value])?;
    }
    Ok(())
}

/// Hard reset and remove untracked files (keep bench marker files).
pub fn reset_worktree_clean(git: &Path, repo: &Path) -> Result<()> {
    run_git(git, repo, &["reset", "--hard", "HEAD"])?;
    run_git(git, repo, &["clean", "-fd", "-q"])?;
    pin_worktree_bench_config(git, repo)?;
    Ok(())
}

/// Modify ~1% of tracked paths and add ~1% untracked files (status-dirty shape).
pub fn apply_one_percent_dirty(git: &Path, repo: &Path) -> Result<()> {
    reset_worktree_clean(git, repo)?;
    let paths = modified_path_list(git, repo)?;
    modify_tracked_files(repo, &paths, paths.len())?;
    let total = list_tracked_files(git, repo)?.len();
    let untracked = (total / MODIFY_FRACTION_DENOM).max(1);
    for i in 0..untracked {
        fs::write(
            repo.join(format!(".grit-bench-untracked-{i}.txt")),
            format!("untracked {i}\n"),
        )?;
    }
    Ok(())
}

/// Modify ~1% of tracked paths only (unstaged; index matches `HEAD`).
pub fn apply_one_percent_modified(git: &Path, repo: &Path) -> Result<()> {
    reset_worktree_clean(git, repo)?;
    let paths = modified_path_list(git, repo)?;
    modify_tracked_files(repo, &paths, paths.len())?;
    Ok(())
}

/// Stage the deterministic modified path set with git (for commit prepare).
pub fn stage_modified_set(git: &Path, repo: &Path) -> Result<()> {
    let paths = modified_path_list(git, repo)?;
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add".to_string()];
    for p in &paths {
        args.push(p.display().to_string());
    }
    run_git(
        git,
        repo,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    Ok(())
}

/// Clone or copy a registry fixture into the bench scratch dir and record modify targets.
pub fn ensure_worktree_bench_repo(
    git: &Path,
    kind: FixtureKind,
    offline: bool,
) -> Result<(PathBuf, FixtureRecord)> {
    let (cached, record) = fixture_registry::ensure_cached(git, kind, offline)?;
    let scratch = scratch_dir();
    fs::create_dir_all(&scratch).context("create scratch dir")?;
    let dest = scratch.join(format!("worktree-run-{}", kind.cache_name()));
    if dest.exists() {
        remove_dir_robust(&dest);
    }
    let parent = dest.parent().context("scratch parent")?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .context("dest name")?;
    let src = cached.to_str().context("cached utf-8")?;
    if kind == FixtureKind::GitGit {
        run_git(git, parent, &["clone", "-q", "--local", src, name])?;
    } else {
        fixture_registry::copy_for_use(git, &cached, &dest)?;
    }
    pin_worktree_bench_config(git, &dest)?;

    let mut rel = list_tracked_files(git, &dest)?
        .into_iter()
        .filter(|p| p.to_string_lossy() != ".gitmodules")
        .collect::<Vec<_>>();
    rel.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let modify_count = (rel.len() / MODIFY_FRACTION_DENOM).max(1).min(rel.len());
    write_modify_count(&dest, modify_count)?;
    write_paths_marker(&dest, &rel[..modify_count])?;
    Ok((dest, record))
}

/// Resolve the worktree under test (hyperfine `--prepare` runs in a fresh process).
pub fn active_worktree_repo() -> PathBuf {
    if let Ok(p) = std::env::var("GRIT_BENCH_CAMPAIGN_CWD") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let marker = scratch_dir().join(ACTIVE_REPO_MARKER);
    if marker.is_file() {
        if let Ok(body) = fs::read_to_string(&marker) {
            let p = body.trim();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    bench_work_dir()
}

/// Record the active repo for hyperfine prepare hooks during a worktree group run.
pub fn set_worktree_bench_cwd(repo: &Path) -> Result<()> {
    std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", repo);
    fs::create_dir_all(scratch_dir()).context("create scratch dir")?;
    fs::write(
        scratch_dir().join(ACTIVE_REPO_MARKER),
        format!("{}\n", repo.display()),
    )
    .context("write active worktree marker")?;
    Ok(())
}

/// Shell snippet: stage the modified set (paths file written at fixture setup).
pub fn shell_git_add_modified_set(git: &Path) -> String {
    let git_q = crate::shell::shell_quote(&git.to_string_lossy());
    format!(
        r#"set -eu
while IFS= read -r p || [ -n "$p" ]; do
  [ -n "$p" ] || continue
  {git_q} add -- "$p" || exit $?
done < {paths} > /dev/null 2>&1"#,
        paths = shell_quote(&scratch_dir().join(PATHS_MARKER).to_string_lossy(),)
    )
}

/// Redirect a single CLI invocation to `/dev/null`.
pub fn shell_redirect_devnull(program: &Path, args: &[&str]) -> String {
    let argv: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    format!(
        "{} > /dev/null 2>&1",
        crate::shell::shell_command(program, &argv)
    )
}

/// Prepare hook entrypoints (hyperfine `--prepare`).
pub fn prepare_status_clean(git: &Path) -> Result<()> {
    reset_worktree_clean(git, &active_worktree_repo())
}

pub fn prepare_status_dirty(git: &Path) -> Result<()> {
    apply_one_percent_dirty(git, &active_worktree_repo())
}

pub fn prepare_add(git: &Path) -> Result<()> {
    apply_one_percent_modified(git, &active_worktree_repo())
}

pub fn prepare_commit(git: &Path) -> Result<()> {
    let repo = active_worktree_repo();
    apply_one_percent_modified(git, &repo)?;
    stage_modified_set(git, &repo)
}

pub fn prepare_diff(git: &Path) -> Result<()> {
    apply_one_percent_modified(git, &active_worktree_repo())
}
