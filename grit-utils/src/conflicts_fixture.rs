//! Synthetic fixtures for merge / pick / revert conflict benchmark scenarios.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::hot_path_fixture::{
    assert_pick_merge_sibling_topology, commit_main_diverge, extend_history, list_tracked_files,
    meta_path, populate_tree, rev_parse, run_git, setup_pick_merge_fixture_in,
    setup_pick_series_fixture, setup_pick_series_fixture_in, touch_paths_for_size, HotPathMeta,
    HotPathRepoSpec,
};

/// Topic branch for campaign conflict rows (distinct from `campaign-topic`).
pub(crate) const BENCH_CONFLICTS_TOPIC: &str = "bench-conflicts-topic";

/// Extra metadata for conflict-operation benchmarks (stored beside hot-path meta).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictsMeta {
    #[serde(flatten)]
    pub base: HotPathMeta,
    /// Merge commit on `main_branch` (two-parent) for `revert-mainline`.
    pub merge_commit: String,
    /// Number of paths edited on both sides for `merge-conflict-continue`.
    pub conflict_path_count: usize,
    /// When set, `merge-conflict-continue` resets here instead of [`HotPathMeta::main_tip`]
    /// (campaign fixtures keep a clean `main_tip` for merge-no-ff / pick-range).
    #[serde(default)]
    pub conflict_main_tip: String,
}

pub fn conflicts_meta_path(repo: &Path) -> PathBuf {
    repo.join(".git/grit-bench-conflicts.json")
}

pub fn load_conflicts_meta(repo: &Path) -> Result<ConflictsMeta> {
    let raw = fs::read_to_string(conflicts_meta_path(repo)).context("read conflicts meta")?;
    serde_json::from_str(&raw).context("parse conflicts meta")
}

fn write_conflicts_meta_files(
    dir: &Path,
    meta: &ConflictsMeta,
    sync_hot_path_meta: bool,
) -> Result<()> {
    let git_dir = dir.join(".git");
    fs::create_dir_all(&git_dir)?;
    fs::write(
        conflicts_meta_path(dir),
        serde_json::to_string_pretty(meta)?,
    )?;
    if sync_hot_path_meta {
        fs::write(meta_path(dir), serde_json::to_string_pretty(&meta.base)?)?;
    }
    Ok(())
}

/// Write conflicts metadata and mirror [`HotPathMeta`] into the hot-path meta file (standalone fixtures).
pub(crate) fn persist_conflicts_meta(dir: &Path, meta: &ConflictsMeta) -> Result<()> {
    write_conflicts_meta_files(dir, meta, true)
}

/// Write conflicts metadata without clobbering campaign hot-path meta on the same worktree.
pub(crate) fn persist_conflicts_meta_campaign(dir: &Path, meta: &ConflictsMeta) -> Result<()> {
    write_conflicts_meta_files(dir, meta, false)
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

/// Clean sibling-branch fixture (same topology as pick/merge hot paths).
pub fn setup_conflicts_clean_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
) -> Result<(PathBuf, ConflictsMeta)> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    setup_conflicts_clean_fixture_in(git, &dir, spec, touch_paths)
}

pub fn setup_conflicts_clean_fixture_in(
    git: &Path,
    dir: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
) -> Result<(PathBuf, ConflictsMeta)> {
    let (_repo, base) = setup_pick_merge_fixture_in(git, dir, spec, touch_paths)?;
    let meta = ConflictsMeta {
        base,
        merge_commit: String::new(),
        conflict_path_count: 0,
        conflict_main_tip: String::new(),
    };
    persist_conflicts_meta(dir, &meta)?;
    Ok((dir.to_path_buf(), meta))
}

/// Topic and `main` both modify the same paths so merge stops with conflicts.
pub fn setup_conflicts_merge_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
    overlap_paths: usize,
) -> Result<(PathBuf, ConflictsMeta)> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    setup_conflicts_merge_fixture_in(git, &dir, spec, touch_paths, overlap_paths)
}

pub fn setup_conflicts_merge_fixture_in(
    git: &Path,
    dir: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
    overlap_paths: usize,
) -> Result<(PathBuf, ConflictsMeta)> {
    if dir.exists() {
        remove_dir_robust(dir);
    }
    fs::create_dir_all(dir)?;
    run_git(git, dir, &["init", "-q"])?;
    populate_tree(dir, spec.file_count, spec.dir_count)?;
    run_git(git, dir, &["add", "-A"])?;
    run_git(git, dir, &["commit", "-q", "-m", "initial"])?;
    if spec.history_commits > 1 {
        extend_history(git, dir, spec.history_commits.saturating_sub(1))?;
    }

    let main_branch = "main".to_string();
    let topic_branch = "topic".to_string();
    run_git(git, dir, &["branch", "-q", &main_branch])?;
    let base_commit = rev_parse(git, dir, "HEAD")?;
    run_git(git, dir, &["checkout", "-q", "-b", &topic_branch])?;

    let files = list_tracked_files(dir)?;
    let n = touch_paths.min(files.len());
    let targets: Vec<PathBuf> = files.iter().take(n).cloned().collect();
    write_files(dir, &targets, "topic-touch")?;
    run_git(git, dir, &["add", "-A"])?;
    run_git(git, dir, &["commit", "-q", "-m", "topic tip"])?;
    let pick_commit = rev_parse(git, dir, "HEAD")?;

    run_git(git, dir, &["checkout", "-q", &main_branch])?;
    let overlap = overlap_paths.min(targets.len()).max(1);
    let conflict_targets: Vec<PathBuf> = targets.iter().take(overlap).cloned().collect();
    write_files(dir, &conflict_targets, "main-conflict")?;
    run_git(git, dir, &["add", "-A"])?;
    run_git(git, dir, &["commit", "-q", "-m", "main overlap"])?;
    let main_tip = commit_main_diverge(git, dir, &files)?;

    let base = HotPathMeta {
        branch_a: main_branch.clone(),
        branch_b: topic_branch.clone(),
        main_branch,
        topic_branch,
        base_commit,
        main_tip,
        pick_commit,
        pick_series_commits: Vec::new(),
        wide: false,
        wide_branch_b: None,
    };
    assert_pick_merge_sibling_topology(git, dir, &base)?;
    let meta = ConflictsMeta {
        base,
        merge_commit: String::new(),
        conflict_path_count: overlap,
        conflict_main_tip: String::new(),
    };
    persist_conflicts_meta(dir, &meta)?;
    Ok((dir.to_path_buf(), meta))
}

/// Record a completed `--no-ff` merge on `main` for revert-mainline scenarios.
pub fn setup_conflicts_revert_mainline_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
) -> Result<(PathBuf, ConflictsMeta)> {
    let dir = scratch_dir();
    remove_dir_robust(&dir);
    setup_conflicts_revert_mainline_fixture_in(git, &dir, spec, touch_paths)
}

pub fn setup_conflicts_revert_mainline_fixture_in(
    git: &Path,
    dir: &Path,
    spec: HotPathRepoSpec,
    touch_paths: usize,
) -> Result<(PathBuf, ConflictsMeta)> {
    let (_repo, mut meta) = setup_conflicts_clean_fixture_in(git, dir, spec, touch_paths)?;
    run_git(git, dir, &["checkout", "-q", &meta.base.main_branch])?;
    run_git(
        git,
        dir,
        &[
            "merge",
            "--no-ff",
            "-q",
            "--no-edit",
            &meta.base.topic_branch,
        ],
    )?;
    meta.merge_commit = rev_parse(git, dir, "HEAD")?;
    if meta.merge_commit == meta.base.main_tip {
        bail!("expected merge commit on main after --no-ff merge");
    }
    persist_conflicts_meta(dir, &meta)?;
    Ok((dir.to_path_buf(), meta))
}

/// Twenty-commit topic series (shared with pick-range scenarios).
pub fn setup_conflicts_pick_range_fixture(
    git: &Path,
    spec: HotPathRepoSpec,
) -> Result<(PathBuf, ConflictsMeta)> {
    let (dir, base) = setup_pick_series_fixture(git, spec)?;
    let meta = ConflictsMeta {
        base,
        merge_commit: String::new(),
        conflict_path_count: 0,
        conflict_main_tip: String::new(),
    };
    persist_conflicts_meta(&dir, &meta)?;
    Ok((dir, meta))
}

pub fn setup_conflicts_pick_range_fixture_in(
    git: &Path,
    dir: &Path,
    spec: HotPathRepoSpec,
) -> Result<(PathBuf, ConflictsMeta)> {
    let (_repo, base) = setup_pick_series_fixture_in(git, dir, spec)?;
    let meta = ConflictsMeta {
        base,
        merge_commit: String::new(),
        conflict_path_count: 0,
        conflict_main_tip: String::new(),
    };
    persist_conflicts_meta(dir, &meta)?;
    Ok((dir.to_path_buf(), meta))
}

fn prepare_conflicts_reset_main(git: &Path, repo: &Path, tip: &str) -> Result<()> {
    let meta = load_conflicts_meta(repo)?;
    run_git(git, repo, &["checkout", "-q", &meta.base.main_branch])?;
    run_git(git, repo, &["reset", "-q", "--hard", tip])?;
    Ok(())
}

fn prepare_conflicts_to_main_tip(git: &Path, repo: &Path) -> Result<()> {
    let meta = load_conflicts_meta(repo)?;
    prepare_conflicts_reset_main(git, repo, &meta.base.main_tip)
}

pub fn prepare_conflicts_merge_no_ff(git: &Path, repo: &Path) -> Result<()> {
    prepare_conflicts_to_main_tip(git, repo)
}

pub fn prepare_conflicts_merge_conflict_continue(git: &Path, repo: &Path) -> Result<()> {
    let meta = load_conflicts_meta(repo)?;
    let tip = if meta.conflict_main_tip.is_empty() {
        meta.base.main_tip.as_str()
    } else {
        meta.conflict_main_tip.as_str()
    };
    prepare_conflicts_reset_main(git, repo, tip)
}

pub fn prepare_conflicts_pick_range(git: &Path, repo: &Path) -> Result<()> {
    prepare_conflicts_to_main_tip(git, repo)
}

pub fn prepare_conflicts_revert_single(git: &Path, repo: &Path) -> Result<()> {
    let meta = load_conflicts_meta(repo)?;
    run_git(git, repo, &["checkout", "-q", &meta.base.main_branch])?;
    run_git(git, repo, &["reset", "-q", "--hard", &meta.base.main_tip])?;
    run_git(
        git,
        repo,
        &["cherry-pick", "--no-edit", &meta.base.pick_commit],
    )?;
    Ok(())
}

pub fn prepare_conflicts_revert_mainline(git: &Path, repo: &Path) -> Result<()> {
    let meta = load_conflicts_meta(repo)?;
    if meta.merge_commit.is_empty() {
        bail!("revert-mainline fixture missing merge_commit");
    }
    run_git(git, repo, &["checkout", "-q", &meta.base.main_branch])?;
    run_git(git, repo, &["reset", "-q", "--hard", &meta.merge_commit])?;
    Ok(())
}

/// Overlap path count for conflict merges at a given repo size.
pub fn conflict_overlap_for_size(file_count: usize) -> usize {
    if file_count >= 10_000 {
        200
    } else {
        (file_count / 20).max(10)
    }
}

pub fn conflicts_spec(file_count: usize) -> HotPathRepoSpec {
    HotPathRepoSpec::for_size(file_count)
}

pub fn conflicts_touch_paths(file_count: usize) -> usize {
    touch_paths_for_size(file_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::resolve_binary;
    use crate::hot_path_fixture::rev_parse;

    fn git_bin() -> PathBuf {
        resolve_binary("git", None).expect("git")
    }

    struct EnvVarGuard {
        key: &'static str,
        prior: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prior }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn conflict_merge_fixture_reports_overlap() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 400,
            dir_count: 4,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let overlap = conflict_overlap_for_size(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        let (_repo, meta) =
            setup_conflicts_merge_fixture_in(&git, dir.path(), spec, touch, overlap)
                .expect("conflict fixture");
        assert!(meta.conflict_path_count >= 10);
        assert_pick_merge_sibling_topology(&git, dir.path(), &meta.base).expect("topology");
    }

    #[test]
    fn merge_conflict_fixture_writes_meta_and_prepares() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let overlap = conflict_overlap_for_size(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = EnvVarGuard::set("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
        setup_conflicts_merge_fixture_in(&git, dir.path(), spec, touch, overlap).expect("setup");
        assert!(conflicts_meta_path(dir.path()).is_file());
        prepare_conflicts_merge_conflict_continue(&git, dir.path()).expect("prepare");
    }

    #[test]
    fn merge_conflict_prepare_after_setup() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let overlap = conflict_overlap_for_size(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = EnvVarGuard::set("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
        setup_conflicts_merge_fixture_in(&git, dir.path(), spec, touch, overlap).expect("setup");
        prepare_conflicts_merge_conflict_continue(&git, dir.path()).expect("prepare");
    }

    #[test]
    fn merge_no_ff_prepare_after_setup() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = EnvVarGuard::set("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
        setup_conflicts_clean_fixture_in(&git, dir.path(), spec, touch).expect("setup");
        prepare_conflicts_merge_no_ff(&git, dir.path()).expect("prepare");
    }

    #[test]
    fn revert_single_prepare_cherry_picks_topic() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        let _cwd = EnvVarGuard::set("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
        setup_conflicts_clean_fixture_in(&git, dir.path(), spec, touch).expect("setup");
        let meta = load_conflicts_meta(dir.path()).expect("meta");
        prepare_conflicts_revert_single(&git, dir.path()).expect("prepare");
        let parent = rev_parse(&git, dir.path(), "HEAD^").expect("parent");
        assert_eq!(parent, meta.base.main_tip);
    }

    #[test]
    fn revert_mainline_fixture_records_merge_commit() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        let (_repo, meta) =
            setup_conflicts_revert_mainline_fixture_in(&git, dir.path(), spec, touch)
                .expect("mainline");
        assert!(!meta.merge_commit.is_empty());
        assert_ne!(meta.merge_commit, meta.base.main_tip);
    }
}
