//! Synthetic fixtures for merge / pick / revert conflict benchmark scenarios.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::fixture::{remove_dir_robust, scratch_dir};
use crate::hot_path_fixture::{
    assert_pick_merge_sibling_topology, commit_main_diverge, extend_history, list_tracked_files,
    meta_path, populate_tree, prepare_pick, prepare_pick_series, rev_parse, run_git,
    setup_pick_merge_fixture_in, setup_pick_series_fixture, setup_pick_series_fixture_in,
    touch_paths_for_size, HotPathMeta, HotPathRepoSpec,
};

/// Extra metadata for conflict-operation benchmarks (stored beside hot-path meta).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictsMeta {
    #[serde(flatten)]
    pub base: HotPathMeta,
    /// Merge commit on `main_branch` (two-parent) for `revert-mainline`.
    pub merge_commit: String,
    /// Number of paths edited on both sides for `merge-conflict-continue`.
    pub conflict_path_count: usize,
}

pub fn conflicts_meta_path(repo: &Path) -> PathBuf {
    repo.join(".git/grit-bench-conflicts.json")
}

pub fn load_conflicts_meta(repo: &Path) -> Result<ConflictsMeta> {
    let raw = fs::read_to_string(conflicts_meta_path(repo)).context("read conflicts meta")?;
    serde_json::from_str(&raw).context("parse conflicts meta")
}

fn write_conflicts_meta(dir: &Path, meta: &ConflictsMeta) -> Result<()> {
    let git_dir = dir.join(".git");
    fs::create_dir_all(&git_dir)?;
    fs::write(
        conflicts_meta_path(dir),
        serde_json::to_string_pretty(meta)?,
    )?;
    fs::write(meta_path(dir), serde_json::to_string_pretty(&meta.base)?)?;
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
    };
    write_conflicts_meta(dir, &meta)?;
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
    };
    write_conflicts_meta(dir, &meta)?;
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
    write_conflicts_meta(dir, &meta)?;
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
    };
    write_conflicts_meta(&dir, &meta)?;
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
    };
    write_conflicts_meta(dir, &meta)?;
    Ok((dir.to_path_buf(), meta))
}

pub fn prepare_conflicts_merge_no_ff(git: &Path, repo: &Path) -> Result<()> {
    prepare_pick(git, repo)
}

pub fn prepare_conflicts_merge_conflict_continue(git: &Path, repo: &Path) -> Result<()> {
    prepare_pick(git, repo)
}

pub fn prepare_conflicts_pick_range(git: &Path, repo: &Path) -> Result<()> {
    prepare_pick_series(git, repo)
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

    fn git_bin() -> PathBuf {
        resolve_binary("git", None).expect("git")
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
    fn revert_single_matches_git() {
        let git = git_bin();
        let grit = resolve_binary("grit", None).expect("grit");
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let grit_dir = tempfile::tempdir().expect("grit dir");
        let git_dir = tempfile::tempdir().expect("git dir");
        setup_conflicts_clean_fixture_in(&git, grit_dir.path(), spec, touch).expect("setup grit");
        setup_conflicts_clean_fixture_in(&git, git_dir.path(), spec, touch).expect("setup git");
        prepare_conflicts_revert_single(&git, grit_dir.path()).expect("prep grit");
        prepare_conflicts_revert_single(&git, git_dir.path()).expect("prep git");
        let grit_status = std::process::Command::new(&grit)
            .current_dir(grit_dir.path())
            .args(["revert", "HEAD"])
            .status()
            .expect("grit revert");
        let git_status = std::process::Command::new(&git)
            .current_dir(git_dir.path())
            .args(["revert", "--no-edit", "HEAD"])
            .status()
            .expect("git revert");
        assert!(grit_status.success(), "grit revert failed");
        assert!(git_status.success(), "git revert failed");
    }

    #[test]
    fn factory_merge_conflict_repo_has_meta() {
        let git = git_bin();
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let overlap = conflict_overlap_for_size(spec.file_count);
        let repo = std::path::PathBuf::from("/tmp/grit-bench-scratch/merge-conflict-continue-200");
        if repo.exists() {
            crate::fixture::remove_dir_robust(&repo);
        }
        setup_conflicts_merge_fixture_in(&git, &repo, spec, touch, overlap).expect("setup");
        assert!(conflicts_meta_path(&repo).is_file());
        prepare_conflicts_merge_conflict_continue(&git, &repo).expect("prepare");
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
        std::env::set_var("GRIT_BENCH_SCRATCH", dir.path());
        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
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
        std::env::set_var("GRIT_BENCH_SCRATCH", dir.path());
        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
        setup_conflicts_clean_fixture_in(&git, dir.path(), spec, touch).expect("setup");
        prepare_conflicts_merge_no_ff(&git, dir.path()).expect("prepare");
    }

    #[test]
    fn merge_conflict_continue_script_exits_zero() {
        let git = git_bin();
        let grit = resolve_binary("grit", None).expect("grit");
        let spec = HotPathRepoSpec {
            file_count: 200,
            dir_count: 2,
            history_commits: 5,
            fsmonitor: false,
        };
        let touch = conflicts_touch_paths(spec.file_count);
        let overlap = conflict_overlap_for_size(spec.file_count);
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("GRIT_BENCH_SCRATCH", dir.path());
        std::env::set_var("GRIT_BENCH_CAMPAIGN_CWD", dir.path());
        setup_conflicts_merge_fixture_in(&git, dir.path(), spec, touch, overlap).expect("setup");
        prepare_conflicts_merge_conflict_continue(&git, dir.path()).expect("prepare");
        let topic = load_conflicts_meta(dir.path())
            .expect("meta")
            .base
            .topic_branch;
        let grit_status = std::process::Command::new(&grit)
            .current_dir(dir.path())
            .args(["merge", &topic])
            .status()
            .expect("grit merge");
        assert!(!grit_status.success(), "merge should stop on conflicts");
        let finish = std::process::Command::new(&grit)
            .current_dir(dir.path())
            .args(["restore", "--theirs", "."])
            .status()
            .expect("restore");
        assert!(finish.success(), "restore --theirs failed");
        let add = std::process::Command::new(&grit)
            .current_dir(dir.path())
            .args(["add", "."])
            .status()
            .expect("add");
        assert!(add.success(), "add failed");
        let cont = std::process::Command::new(&grit)
            .current_dir(dir.path())
            .args(["merge", "--continue"])
            .status()
            .expect("continue");
        assert!(cont.success(), "merge --continue failed");
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
