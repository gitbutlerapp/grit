//! Synthetic repositories for switch / pick / merge hot-path benchmarks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::bench_env::{empty_global_config_path, enable_fsmonitor, write_fsmonitor_hook};
use crate::fixture::{remove_dir_robust, scratch_dir};

/// Parameters for a hot-path benchmark repository.
#[derive(Debug, Clone, Copy)]
pub struct HotPathRepoSpec {
    pub file_count: usize,
    pub dir_count: usize,
    pub history_commits: usize,
    pub fsmonitor: bool,
}

impl HotPathRepoSpec {
    /// Large fixture: 10k files, 100 dirs, 1000 commits.
    pub fn large() -> Self {
        Self {
            file_count: 10_000,
            dir_count: 100,
            history_commits: 1000,
            fsmonitor: false,
        }
    }

    /// Heavy fixture: 100k files, 1000 dirs, 1000 commits.
    pub fn heavy() -> Self {
        Self {
            file_count: 100_000,
            dir_count: 1000,
            history_commits: 1000,
            fsmonitor: false,
        }
    }

    /// Smaller repo for integration tests (fewer commits for speed).
    pub fn for_size(file_count: usize) -> Self {
        let dir_count = (file_count / 100).max(1);
        let history = if file_count >= 10_000 { 1000 } else { 20 };
        Self {
            file_count,
            dir_count,
            history_commits: history,
            fsmonitor: false,
        }
    }
}

/// Metadata written beside the scratch repo for prepare hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotPathMeta {
    pub branch_a: String,
    pub branch_b: String,
    pub main_branch: String,
    pub topic_branch: String,
    pub base_commit: String,
    pub pick_commit: String,
    pub pick_series_commits: Vec<String>,
    pub wide: bool,
}

pub fn meta_path(repo: &Path) -> PathBuf {
    repo.join(".grit-bench-hot-path.json")
}

pub fn load_meta(repo: &Path) -> Result<HotPathMeta> {
    let raw = fs::read_to_string(meta_path(repo)).context("read hot-path meta")?;
    serde_json::from_str(&raw).context("parse hot-path meta")
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

fn list_tracked_files(repo: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk(repo, repo, &mut files)?;
    Ok(files)
}

fn walk(base: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git") {
            continue;
        }
        if path.is_dir() {
            walk(base, &path, files)?;
        } else if path.extension().is_some_and(|e| e == "txt") {
            if let Ok(rel) = path.strip_prefix(base) {
                files.push(rel.to_path_buf());
            }
        }
    }
    Ok(())
}

fn write_files(repo: &Path, rel_paths: &[PathBuf], suffix: &str) -> Result<()> {
    for rel in rel_paths {
        let path = repo.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, format!("{suffix}\n"))?;
    }
    Ok(())
}

fn populate_tree(repo: &Path, file_count: usize, dir_count: usize) -> Result<()> {
    let files_per_dir = file_count.div_ceil(dir_count.max(1));
    let mut created = 0usize;
    for d in 0..dir_count.max(1) {
        let subdir = repo.join(format!("d{d:04}"));
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
    Ok(())
}

fn extend_history(git: &Path, repo: &Path, extra_commits: usize) -> Result<()> {
    let files = list_tracked_files(repo)?;
    if files.is_empty() {
        return Ok(());
    }
    for i in 0..extra_commits {
        if i % 10 == 0 {
            let rel = &files[i % files.len()];
            write_files(repo, std::slice::from_ref(rel), &format!("history-{i}"))?;
            let rel_str = rel.to_string_lossy();
            run_git(git, repo, &["add", rel_str.as_ref()])?;
            run_git(git, repo, &["commit", "-q", "-m", &format!("history {i}")])?;
        } else {
            run_git(
                git,
                repo,
                &[
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    &format!("history-empty {i}"),
                ],
            )?;
        }
    }
    Ok(())
}

/// Build a repository for switch benchmarks; returns repo path and branch names.
pub fn setup_switch_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
    wide: bool,
) -> Result<(PathBuf, HotPathMeta)> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir)?;
    run_git(git, &dir, &["init", "-q"])?;
    populate_tree(&dir, spec.file_count, spec.dir_count)?;
    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "initial"])?;
    if spec.history_commits > 1 {
        extend_history(git, &dir, spec.history_commits.saturating_sub(1))?;
    }

    let branch_a = "bench-a".to_string();
    let branch_b = "bench-b".to_string();
    run_git(git, &dir, &["branch", "-q", &branch_a])?;
    run_git(git, &dir, &["checkout", "-q", &branch_a])?;

    let files = list_tracked_files(&dir)?;
    let change_count = if wide {
        (files.len() / 10).max(1)
    } else {
        50.min(files.len())
    };
    let mut targets: Vec<PathBuf> = files.iter().take(change_count).cloned().collect();
    if wide {
        let dirs: Vec<PathBuf> = targets
            .iter()
            .filter_map(|p| p.parent().map(|d| d.to_path_buf()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .take((spec.dir_count / 10).max(1))
            .collect();
        for d in &dirs {
            let abs = dir.join(d);
            if abs.is_dir() {
                fs::remove_dir_all(&abs).ok();
            }
        }
        targets.retain(|p| !dirs.iter().any(|d| p.starts_with(d)));
        write_files(&dir, &targets, "wide-b")?;
    } else {
        write_files(&dir, &targets, "branch-b")?;
    }
    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "branch b tip"])?;
    run_git(git, &dir, &["branch", "-q", &branch_b])?;
    run_git(git, &dir, &["checkout", "-q", &branch_a])?;

    if spec.fsmonitor {
        let hook = write_fsmonitor_hook(&dir)?;
        enable_fsmonitor(git, &dir, &hook)?;
        run_git(git, &dir, &["status", "-s"])?;
    }

    let base_commit = rev_parse(git, &dir, "HEAD")?;
    let meta = HotPathMeta {
        branch_a: branch_a.clone(),
        branch_b: branch_b.clone(),
        main_branch: branch_a,
        topic_branch: branch_b.clone(),
        base_commit,
        pick_commit: String::new(),
        pick_series_commits: Vec::new(),
        wide,
    };
    fs::write(meta_path(&dir), serde_json::to_string_pretty(&meta)?)?;
    Ok((dir, meta))
}

/// Pick / merge fixture: `main` at base, `topic` with one large commit.
pub fn setup_pick_merge_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
) -> Result<(PathBuf, HotPathMeta)> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir)?;
    run_git(git, &dir, &["init", "-q"])?;
    populate_tree(&dir, spec.file_count, spec.dir_count)?;
    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "initial"])?;
    if spec.history_commits > 1 {
        extend_history(git, &dir, spec.history_commits.saturating_sub(1))?;
    }

    let main_branch = "main".to_string();
    let topic_branch = "topic".to_string();
    run_git(git, &dir, &["branch", "-q", &main_branch])?;
    let base_commit = rev_parse(git, &dir, "HEAD")?;
    run_git(git, &dir, &["checkout", "-q", "-b", &topic_branch])?;

    let files = list_tracked_files(&dir)?;
    let n = touch_paths.min(files.len());
    let targets: Vec<PathBuf> = files.iter().take(n).cloned().collect();
    write_files(&dir, &targets, "pick-touch")?;
    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "topic tip"])?;
    let pick_commit = rev_parse(git, &dir, "HEAD")?;

    run_git(git, &dir, &["checkout", "-q", &main_branch])?;

    if spec.fsmonitor {
        let hook = write_fsmonitor_hook(&dir)?;
        enable_fsmonitor(git, &dir, &hook)?;
        run_git(git, &dir, &["status", "-s"])?;
    }

    let meta = HotPathMeta {
        branch_a: main_branch.clone(),
        branch_b: topic_branch.clone(),
        main_branch,
        topic_branch: topic_branch.clone(),
        base_commit,
        pick_commit,
        pick_series_commits: Vec::new(),
        wide: false,
    };
    fs::write(meta_path(&dir), serde_json::to_string_pretty(&meta)?)?;
    Ok((dir, meta))
}

/// Pick-series fixture: 20 commits on `topic`.
pub fn setup_pick_series_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
) -> Result<(PathBuf, HotPathMeta)> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    fs::create_dir_all(&dir)?;
    run_git(git, &dir, &["init", "-q"])?;
    populate_tree(&dir, spec.file_count, spec.dir_count)?;
    run_git(git, &dir, &["add", "-A"])?;
    run_git(git, &dir, &["commit", "-q", "-m", "initial"])?;
    if spec.history_commits > 1 {
        extend_history(git, &dir, spec.history_commits.saturating_sub(1))?;
    }

    let main_branch = "main".to_string();
    let topic_branch = "topic".to_string();
    run_git(git, &dir, &["branch", "-q", &main_branch])?;
    let base_commit = rev_parse(git, &dir, "HEAD")?;
    run_git(git, &dir, &["checkout", "-q", "-b", &topic_branch])?;

    let files = list_tracked_files(&dir)?;
    let mut series = Vec::new();
    const SERIES_LEN: usize = 20;
    for i in 0..SERIES_LEN {
        let rel = &files[i % files.len()];
        write_files(&dir, std::slice::from_ref(rel), &format!("series-{i}"))?;
        run_git(git, &dir, &["add", "-A"])?;
        run_git(git, &dir, &["commit", "-q", "-m", &format!("series {i}")])?;
        series.push(rev_parse(git, &dir, "HEAD")?);
    }
    let pick_commit = series.last().cloned().unwrap_or_default();

    run_git(git, &dir, &["checkout", "-q", &main_branch])?;

    if spec.fsmonitor {
        let hook = write_fsmonitor_hook(&dir)?;
        enable_fsmonitor(git, &dir, &hook)?;
        run_git(git, &dir, &["status", "-s"])?;
    }

    let meta = HotPathMeta {
        branch_a: main_branch.clone(),
        branch_b: topic_branch.clone(),
        main_branch,
        topic_branch,
        base_commit,
        pick_commit,
        pick_series_commits: series,
        wide: false,
    };
    fs::write(meta_path(&dir), serde_json::to_string_pretty(&meta)?)?;
    Ok((dir, meta))
}

pub fn prepare_switch(git: &Path, repo: &Path) -> Result<()> {
    let meta = load_meta(repo)?;
    run_git(git, repo, &["checkout", "-q", &meta.branch_a])?;
    Ok(())
}

pub fn prepare_pick(git: &Path, repo: &Path) -> Result<()> {
    let meta = load_meta(repo)?;
    run_git(git, repo, &["checkout", "-q", &meta.main_branch])?;
    run_git(git, repo, &["reset", "-q", "--hard", &meta.base_commit])?;
    Ok(())
}

pub fn prepare_merge(git: &Path, repo: &Path) -> Result<()> {
    prepare_pick(git, repo)
}

pub fn prepare_pick_series(git: &Path, repo: &Path) -> Result<()> {
    prepare_pick(git, repo)
}

pub fn touch_paths_for_size(file_count: usize) -> usize {
    if file_count >= 10_000 {
        2000
    } else {
        (file_count / 2).max(10)
    }
}
