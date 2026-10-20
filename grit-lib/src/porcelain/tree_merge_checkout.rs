//! Apply a clean three-way tree merge to the worktree and index with one tree diff.
//!
//! Pick, revert, and non-conflicted merges already computed [`TreeMergeOutput`]; this module
//! reuses that result for untracked guards, checkout, and sparse index updates instead of
//! running separate full-tree diffs and index reloads.

use std::collections::HashSet;

use crate::diff::{diff_index_to_tree, diff_trees, diff_trees_with_index, DiffEntry};
use crate::error::{Error, Result};
use crate::merge_trees::{
    apply_tree_merge_output_to_worktree_index, tree_changes_for_touched_paths, TreeMergeOutput,
};
use crate::objects::ObjectId;
use crate::porcelain::checkout::{checkout_tree_changes_with_index_opts, CheckoutApplyOptions};
use crate::porcelain::worktree_guard::{
    collect_untracked_for_tree_changes, ensure_no_untracked_overwrite, load_worktree_snapshot,
    local_change_diffs_for_tree_changes, paths_touched_by_tree_changes,
};
use crate::repo::Repository;

/// Refuse pick/merge when the index or worktree differs from `HEAD`, scoped to `touched` paths
/// plus a stat-only scan elsewhere.
pub fn ensure_worktree_clean_for_touched_paths(
    repo: &Repository,
    head_tree: &ObjectId,
    touched: &HashSet<Vec<u8>>,
    dirty_message: &str,
    persist_index_stat_refresh: bool,
) -> Result<()> {
    let tree_changes = tree_changes_for_touched_paths(repo, head_tree, touched)?;
    ensure_worktree_clean_for_tree_changes(
        repo,
        &tree_changes,
        dirty_message,
        persist_index_stat_refresh,
    )
}

/// Refuse pick/merge when the index or worktree differs from `HEAD`, scoped to `tree_changes`
/// plus a stat-only scan on tracked paths outside those changes.
///
/// # Errors
///
/// Returns [`Error::Message`] with `dirty_message` when changes are present.
#[allow(clippy::unnecessary_mut_passed)]
pub fn ensure_worktree_clean_for_tree_changes(
    repo: &Repository,
    tree_changes: &[DiffEntry],
    dirty_message: &str,
    persist_index_stat_refresh: bool,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    if let Some(work_tree) = repo.work_tree() {
        let _ = crate::porcelain::index_from_tree::refresh_stage0_index_stats_from_worktree(
            repo,
            work_tree,
            &mut snapshot.index,
        );
    }
    let index_path = repo.index_path_for_env()?;
    let staged_all = diff_index_to_tree(
        repo.odb(),
        &mut snapshot.index,
        snapshot.head_tree.as_ref(),
        false,
    )?;
    if !staged_all.is_empty() {
        return Err(Error::Message(dirty_message.into()));
    }
    let (staged_scoped, unstaged, index_changed) = local_change_diffs_for_tree_changes(
        repo,
        &mut snapshot.index,
        snapshot.head_tree,
        tree_changes,
        None,
    )?;
    if persist_index_stat_refresh
        && index_changed
        && repo.try_write_index_at(&index_path, &mut snapshot.index)?
    {
        snapshot.index.source_mtime = crate::index::index_file_mtime(&index_path);
    }
    if !staged_scoped.is_empty() || !unstaged.is_empty() {
        return Err(Error::Message(dirty_message.into()));
    }
    if !crate::porcelain::worktree_guard::worktree_clean_outside_tree_change_paths(
        repo,
        &snapshot.index,
        tree_changes,
    )? {
        return Err(Error::Message(dirty_message.into()));
    }
    Ok(())
}

/// Tree diff from `from_tree` to `merged.result_tree`, preferring index hints when `HEAD` matches.
pub(crate) fn tree_changes_for_clean_merge(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
) -> Result<Vec<DiffEntry>> {
    let to_tree = merged.result_tree;
    if from_tree == &to_tree {
        return Ok(Vec::new());
    }
    let snapshot = load_worktree_snapshot(repo)?;
    let trust_index = snapshot.head_tree == Some(*from_tree)
        && !snapshot.index.entries.iter().any(|e| e.stage() != 0);
    if trust_index {
        diff_trees_with_index(
            repo.odb(),
            &snapshot.index,
            Some(from_tree),
            Some(&to_tree),
            "",
            true,
        )
    } else {
        diff_trees(repo.odb(), Some(from_tree), Some(&to_tree), "")
    }
}

/// Checkout and index update for a clean merge result (no conflict markers).
///
/// Assumes the worktree is clean for [`tree_changes_for_clean_merge`] and untracked guards pass.
///
/// # Errors
///
/// Propagates diff, checkout, and index write failures.
pub fn apply_clean_tree_merge_to_worktree(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
) -> Result<()> {
    let tree_changes = tree_changes_for_clean_merge(repo, from_tree, merged)?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let mut snapshot = load_worktree_snapshot(repo)?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, &tree_changes)?;
    ensure_no_untracked_overwrite(&untracked, &tree_changes)?;

    checkout_tree_changes_with_index_opts(
        repo,
        &tree_changes,
        &mut snapshot.index,
        CheckoutApplyOptions {
            parallel_prepare_blobs: true,
            checkout_result_tree: Some(merged.result_tree),
            ..CheckoutApplyOptions::default()
        },
        None,
    )?;
    apply_tree_merge_output_to_worktree_index(repo, &mut snapshot.index, merged)?;
    repo.write_index(&mut snapshot.index)?;
    Ok(())
}

/// Paths touched by `tree_changes` as UTF-8 strings (for tests and diagnostics).
#[must_use]
pub fn tree_change_path_set(tree_changes: &[DiffEntry]) -> std::collections::BTreeSet<String> {
    paths_touched_by_tree_changes(tree_changes)
}
