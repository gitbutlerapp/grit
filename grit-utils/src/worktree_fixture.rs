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
const BASE_OID_MARKER: &str = ".grit-bench-worktree-base-oid";

/// Branch checked out for git.git worktrees (avoids detached HEAD).
pub const GIT_GIT_BENCH_BRANCH: &str = "grit-bench-worktree";

/// Fraction of tracked files to touch in mutating scenarios (~1%).
pub const MODIFY_FRACTION_DENOM: usize = 100;

/// Return the sorted path list used for ~1% modifications (relative paths).
pub fn modified_path_list(git: &Path, repo: &Path) -> Result<Vec<PathBuf>> {
    let count = read_modify_count()?;
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

fn read_modify_count() -> Result<usize> {
    let (count_path, _) = meta_paths();
    let raw = fs::read_to_string(&count_path).context("read modify count")?;
    raw.trim()
        .parse::<usize>()
        .context("parse modify count marker")
}

fn write_modify_count(count: usize) -> Result<()> {
    let (count_path, _) = meta_paths();
    fs::write(&count_path, format!("{count}\n")).context("write modify count marker")
}

fn write_paths_marker(paths: &[PathBuf]) -> Result<()> {
    let (_, paths_path) = meta_paths();
    let body = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&paths_path, format!("{body}\n")).context("write paths marker")
}

fn write_base_oid(oid: &str) -> Result<()> {
    fs::write(scratch_dir().join(BASE_OID_MARKER), format!("{oid}\n"))
        .context("write base oid marker")
}

/// Pinned `HEAD` oid recorded when the worktree clone was created.
pub fn read_fixture_base_oid() -> Result<String> {
    let raw =
        fs::read_to_string(scratch_dir().join(BASE_OID_MARKER)).context("read base oid marker")?;
    let oid = raw.trim().to_string();
    anyhow::ensure!(!oid.is_empty(), "empty base oid marker");
    Ok(oid)
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

/// Reset branch, index, and worktree to the pinned fixture base commit.
pub fn reset_to_fixture_base(git: &Path, repo: &Path) -> Result<()> {
    let base = read_fixture_base_oid()?;
    run_git(git, repo, &["reset", "--hard", &base])?;
    run_git(git, repo, &["clean", "-fd", "-q"])?;
    pin_worktree_bench_config(git, repo)?;
    Ok(())
}

/// Hard reset and remove untracked files (always returns to fixture base, not tip).
pub fn reset_worktree_clean(git: &Path, repo: &Path) -> Result<()> {
    reset_to_fixture_base(git, repo)
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

/// Modify ~1% of tracked paths only (unstaged; index matches fixture base).
pub fn apply_one_percent_modified(git: &Path, repo: &Path) -> Result<()> {
    reset_worktree_clean(git, repo)?;
    let paths = modified_path_list(git, repo)?;
    modify_tracked_files(repo, &paths, paths.len())?;
    Ok(())
}

/// Stage the deterministic modified path set with one `git add` invocation.
pub fn stage_modified_set(git: &Path, repo: &Path) -> Result<()> {
    let paths = modified_path_list(git, repo)?;
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add".to_string(), "--".to_string()];
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
        run_git(git, &dest, &["checkout", "-B", GIT_GIT_BENCH_BRANCH])?;
    } else {
        fixture_registry::copy_for_use(git, &cached, &dest)?;
        if kind == FixtureKind::GitGitWt {
            run_git(git, &dest, &["checkout", "-B", GIT_GIT_BENCH_BRANCH])?;
        }
    }
    pin_worktree_bench_config(git, &dest)?;

    let head = fixture_registry::git_head_oid(git, &dest)?;
    write_base_oid(&head)?;

    let mut rel = list_tracked_files(git, &dest)?
        .into_iter()
        .filter(|p| p.to_string_lossy() != ".gitmodules")
        .collect::<Vec<_>>();
    rel.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    let modify_count = (rel.len() / MODIFY_FRACTION_DENOM).max(1).min(rel.len());
    write_modify_count(modify_count)?;
    write_paths_marker(&rel[..modify_count])?;
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

/// Shell snippet: one `git add --` with every path from the paths file.
pub fn shell_git_add_modified_set(git: &Path) -> String {
    shell_add_modified_set(git)
}

/// Shell snippet: one `grit add --` with the same path list as git.
pub fn shell_grit_add_modified_set(grit: &Path) -> String {
    shell_add_modified_set(grit)
}

fn shell_add_modified_set(program: &Path) -> String {
    let prog_q = shell_quote(&program.to_string_lossy());
    let paths_q = shell_quote(&scratch_dir().join(PATHS_MARKER).to_string_lossy());
    format!(
        r#"set -eu
if [ -s {paths_q} ]; then
  tr '\n' '\0' < {paths_q} | xargs -0 {prog_q} add -- > /dev/null 2>&1
fi"#
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
    let head = fixture_registry::git_head_oid(git, &repo)?;
    let base = read_fixture_base_oid()?;
    anyhow::ensure!(
        head == base,
        "commit prepare must leave HEAD at fixture base, got {head} expected {base}"
    );
    Ok(())
}

/// Stage the modified path set and commit (matches grit `commit` staging + commit in one timed run).
pub fn shell_git_add_modified_set_and_commit(git: &Path) -> String {
    format!(
        "set -eu\n{}\n{}",
        shell_git_add_modified_set(git).trim(),
        shell_redirect_devnull(git, &["commit", "-q", "-m", "bench"])
    )
}

pub fn prepare_diff(git: &Path) -> Result<()> {
    apply_one_percent_modified(git, &active_worktree_repo())
}

const RM_DIR_MARKER: &str = ".grit-bench-rm-dir";
const MV_SRC_MARKER: &str = ".grit-bench-mv-src";
const MV_DST_MARKER: &str = ".grit-bench-mv-dst";

fn write_marker(name: &str, value: &str) -> Result<()> {
    fs::write(scratch_dir().join(name), format!("{value}\n")).context("write matrix marker")
}

fn read_marker(name: &str) -> Result<String> {
    let raw = fs::read_to_string(scratch_dir().join(name)).context("read matrix marker")?;
    let v = raw.trim().to_string();
    anyhow::ensure!(!v.is_empty(), "empty matrix marker {name}");
    Ok(v)
}

/// Pick stable rm/mv targets from the first modified-path parent directory.
pub fn record_matrix_path_targets(git: &Path, repo: &Path) -> Result<()> {
    let paths = modified_path_list(git, repo)?;
    let first = paths
        .first()
        .context("matrix paths need at least one tracked file")?;
    let parent = first
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("matrix path parent")?;
    write_marker(RM_DIR_MARKER, &parent.display().to_string())?;
    write_marker(MV_SRC_MARKER, &first.display().to_string())?;
    let dst = parent.join(format!(
        "{}-bench-mv.txt",
        first.file_stem().and_then(|s| s.to_str()).unwrap_or("f")
    ));
    write_marker(MV_DST_MARKER, &dst.display().to_string())?;
    Ok(())
}

pub fn prepare_staged_diff(git: &Path) -> Result<()> {
    let repo = active_worktree_repo();
    apply_one_percent_modified(git, &repo)?;
    stage_modified_set(git, &repo)
}

pub fn prepare_restore_paths(git: &Path) -> Result<()> {
    apply_one_percent_modified(git, &active_worktree_repo())
}

pub fn prepare_rm_dir(git: &Path) -> Result<()> {
    reset_worktree_clean(git, &active_worktree_repo())
}

pub fn prepare_mv_file(git: &Path) -> Result<()> {
    reset_worktree_clean(git, &active_worktree_repo())
}

pub fn prepare_init_empty(_git: &Path) -> Result<()> {
    let repo = active_worktree_repo();
    if repo.exists() {
        remove_dir_robust(&repo);
    }
    fs::create_dir_all(&repo).context("recreate empty init directory")?;
    Ok(())
}

pub fn prepare_clean_untracked(git: &Path) -> Result<()> {
    let repo = active_worktree_repo();
    reset_worktree_clean(git, &repo)?;
    for i in 0..3 {
        fs::write(
            repo.join(format!(".grit-bench-clean-untracked-{i}.txt")),
            "clean bench\n",
        )?;
    }
    Ok(())
}

/// Shell snippet: restore the deterministic modified path set.
pub fn shell_restore_modified_set(program: &Path) -> String {
    let prog_q = shell_quote(&program.to_string_lossy());
    let paths_q = shell_quote(&scratch_dir().join(PATHS_MARKER).to_string_lossy());
    format!(
        r#"set -eu
if [ -s {paths_q} ]; then
  tr '\n' '\0' < {paths_q} | xargs -0 {prog_q} restore -- > /dev/null 2>&1
fi"#
    )
}

pub fn shell_rm_dir_git(program: &Path) -> String {
    let dir = read_marker(RM_DIR_MARKER).unwrap_or_else(|_| "data/d0000".into());
    shell_redirect_devnull(program, &["rm", "-r", &dir])
}

pub fn shell_rm_dir_grit(program: &Path) -> String {
    let dir = read_marker(RM_DIR_MARKER).unwrap_or_else(|_| "data/d0000".into());
    shell_redirect_devnull(program, &["rm", &dir])
}

pub fn shell_mv_file(program: &Path) -> String {
    let src = read_marker(MV_SRC_MARKER).unwrap_or_else(|_| "data/d0000/f0000.txt".into());
    let dst = read_marker(MV_DST_MARKER).unwrap_or_else(|_| "data/d0000/f_bench_mv.txt".into());
    shell_redirect_devnull(program, &["mv", &src, &dst])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_shell_uses_single_invocation_with_path_array() {
        let git = Path::new("/usr/bin/git");
        let script = shell_git_add_modified_set(git);
        assert!(
            script.contains("xargs -0") && script.contains(" add --"),
            "expected one xargs-driven add: {script}"
        );
        assert!(
            !script.contains(" add -- \"$p\""),
            "must not invoke add once per path: {script}"
        );
    }
}
