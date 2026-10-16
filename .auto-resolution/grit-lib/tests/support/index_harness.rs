//! Shared helpers for index / sparse-checkout git compatibility tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::status::{format_porcelain_v2, status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

use super::history_harness::{git, git_ls_files_stage, HistoryRepo};

/// Run `git fsck --strict` in `worktree` (panics on failure).
pub fn assert_git_fsck_strict(worktree: &Path) {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["fsck", "--strict"])
        .env(
            "GIT_CONFIG_GLOBAL",
            super::refs_harness::null_device_for_tests(),
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            super::refs_harness::null_device_for_tests(),
        )
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// First column of `git ls-files -v` (skip-worktree / assume-unchanged markers).
pub fn git_ls_files_verbose_map(worktree: &Path) -> BTreeMap<String, char> {
    let out = git(worktree, &["ls-files", "-v"]);
    let mut map = BTreeMap::new();
    for line in out.lines() {
        let Some((tag, path)) = line.split_once(' ') else {
            continue;
        };
        let ch = tag.chars().next().unwrap_or('H');
        map.insert(path.to_owned(), ch);
    }
    map
}

/// Tags from `git ls-files -t` (`H` tracked, `S` skip-worktree, etc.).
pub fn git_ls_files_tag_map(worktree: &Path) -> BTreeMap<String, char> {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["ls-files", "-t"])
        .env(
            "GIT_CONFIG_GLOBAL",
            super::refs_harness::null_device_for_tests(),
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            super::refs_harness::null_device_for_tests(),
        )
        .output()
        .expect("git ls-files -t");
    assert!(
        out.status.success(),
        "git ls-files -t: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let Some((tag, path)) = line.split_once(' ') else {
            continue;
        };
        let ch = tag.chars().next().unwrap_or('H');
        map.insert(path.to_owned(), ch);
    }
    map
}

/// Non-header lines from `git status --porcelain=v2`.
pub fn git_status_porcelain_v2_lines(worktree: &Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(["status", "--porcelain=v2"])
        .env(
            "GIT_CONFIG_GLOBAL",
            super::refs_harness::null_device_for_tests(),
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            super::refs_harness::null_device_for_tests(),
        )
        .output()
        .expect("git status");
    assert!(
        out.status.success(),
        "git status --porcelain=v2: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Grit status rendered as porcelain v2 (same subset git uses for ordinary repos).
pub fn grit_status_porcelain_v2_lines(repo: &Repository) -> Vec<String> {
    let model = status(repo, &StatusOptions::default(), &mut NullProgress).expect("grit status");
    format_porcelain_v2(&model, repo.odb(), repo.work_tree())
}

pub fn assert_porcelain_v2_matches_git(worktree: &Path, repo: &Repository) {
    let git_lines = git_status_porcelain_v2_lines(worktree);
    let grit_lines = grit_status_porcelain_v2_lines(repo);
    assert_eq!(
        grit_lines, git_lines,
        "grit porcelain v2 must match git for this scenario"
    );
}

/// Stage-0 view of grit index entries keyed by path.
pub fn grit_index_stage0_map(index: &Index) -> BTreeMap<String, (u32, ObjectId)> {
    index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                (e.mode, e.oid),
            )
        })
        .collect()
}

pub fn assert_grit_index_matches_git_ls_files(worktree: &Path, index: &Index) {
    let git_rows = git_ls_files_stage(worktree);
    let grit_map = grit_index_stage0_map(index);
    for (path, stage, mode, oid) in &git_rows {
        assert_eq!(
            *stage, 0,
            "compare stage-0 only in this helper (path {path})"
        );
        let mode_num = u32::from_str_radix(mode.trim_start_matches('0'), 8).unwrap_or(0);
        let grit = grit_map
            .get(path)
            .unwrap_or_else(|| panic!("grit index missing path {path}"));
        assert_eq!(grit.0, mode_num, "mode mismatch on {path}");
        assert_eq!(grit.1, *oid, "oid mismatch on {path}");
    }
    assert_eq!(
        grit_map.len(),
        git_rows.iter().filter(|(_, s, _, _)| *s == 0).count()
    );
}

pub fn grit_skip_worktree_map(index: &Index) -> BTreeMap<String, bool> {
    index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| {
            (
                String::from_utf8_lossy(&e.path).into_owned(),
                e.skip_worktree(),
            )
        })
        .collect()
}

/// Clear skip-worktree on every index entry (tests start from the wrong state on purpose).
pub fn clear_all_skip_worktree(index: &mut Index) {
    for entry in index.entries_mut() {
        entry.set_skip_worktree(false);
    }
}

/// Compare grit skip-worktree bits to a saved `git ls-files -t` map (`S` = skip-worktree).
pub fn assert_skip_worktree_matches_tag_oracle(index: &Index, oracle: &BTreeMap<String, char>) {
    let grit = grit_skip_worktree_map(index);
    let paths: BTreeSet<_> = oracle.keys().chain(grit.keys()).cloned().collect();
    for path in paths {
        let expect = matches!(oracle.get(&path).copied(), Some('S' | 's'));
        assert_eq!(
            grit.get(&path).copied().unwrap_or(false),
            expect,
            "skip-worktree mismatch on {path} (oracle tag {:?})",
            oracle.get(&path)
        );
    }
}

pub fn assert_skip_worktree_matches_git(worktree: &Path, index: &Index) {
    let git_v = git_ls_files_verbose_map(worktree);
    let grit = grit_skip_worktree_map(index);
    let paths: BTreeSet<_> = git_v.keys().chain(grit.keys()).cloned().collect();
    for path in paths {
        let git_skip = matches!(git_v.get(&path).copied(), Some('S' | 's'));
        assert_eq!(
            grit.get(&path).copied().unwrap_or(false),
            git_skip,
            "skip-worktree mismatch on {path}"
        );
    }
}

/// Populate a multi-directory tree and create an initial commit.
pub fn seed_multidir_repo(repo: &HistoryRepo) {
    let wt = repo.worktree();
    for d in ["a", "b", "c/d"] {
        std::fs::create_dir_all(wt.join(d)).expect("mkdir");
    }
    for (dir, n) in [("a", 5), ("b", 5), ("c/d", 3)] {
        for i in 0..n {
            std::fs::write(
                wt.join(dir).join(format!("f{i}.txt")),
                format!("{dir}/{i}\n"),
            )
            .expect("write");
        }
    }
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-m", "seed"]);
}
