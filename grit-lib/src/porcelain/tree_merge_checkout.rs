//! Apply a clean three-way tree merge to the worktree and index with one tree diff.
//!
//! Pick and revert already computed [`TreeMergeOutput`]; this module reuses that result for
//! untracked guards, worktree checkout, and sparse index updates instead of separate full-tree diffs.

use crate::diff::{diff_trees, diff_trees_with_index, DiffEntry};
use crate::error::{Error, Result};
use crate::index::IndexLock;
use crate::merge_trees::{apply_tree_merge_output_to_worktree_index, TreeMergeOutput};
use crate::objects::ObjectId;
use crate::porcelain::checkout::{
    checkout_tree_changes_with_index_opts, CheckoutApplyOptions, CheckoutIndexWrite,
};
use crate::porcelain::worktree_guard::{
    collect_untracked_for_tree_changes, ensure_no_untracked_overwrite, load_worktree_snapshot,
    WorktreeSnapshot,
};
use crate::repo::Repository;

fn tree_changes_for_clean_merge_with_snapshot(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
    snapshot: &WorktreeSnapshot,
) -> Result<Vec<DiffEntry>> {
    let to_tree = merged.result_tree;
    if from_tree == &to_tree {
        return Ok(Vec::new());
    }
    // Wide indexes: index-hinted diffs scan every entry; tree-only diff is cheaper for small merges.
    let trust_index = snapshot.head_tree == Some(*from_tree)
        && !snapshot.index.entries.iter().any(|e| e.stage() != 0)
        && snapshot.index.entries.len() <= 4096;
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
/// Assumes the worktree is clean and untracked guards pass.
///
/// # Errors
///
/// Propagates diff, checkout, and index write failures.
pub(crate) fn apply_clean_tree_merge_to_worktree(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    apply_clean_tree_merge_to_worktree_with_snapshot(repo, from_tree, merged, &mut snapshot)
}

/// Like [`apply_clean_tree_merge_to_worktree`], but reuses a loaded worktree snapshot across steps.
pub(crate) fn apply_clean_tree_merge_to_worktree_with_snapshot(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
    snapshot: &mut WorktreeSnapshot,
) -> Result<()> {
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let tree_changes =
        tree_changes_for_clean_merge_with_snapshot(repo, from_tree, merged, snapshot)?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, &tree_changes)?;
    ensure_no_untracked_overwrite(&untracked, &tree_changes)?;

    let mut lock = repo.lock_index()?;
    checkout_tree_changes_with_index_opts(
        repo,
        &tree_changes,
        &mut snapshot.index,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
        CheckoutApplyOptions {
            parallel_prepare_blobs: true,
            checkout_result_tree: Some(merged.result_tree),
            ..CheckoutApplyOptions::default()
        },
        None,
        CheckoutIndexWrite::Defer(&lock),
    )?;
    finalize_clean_tree_merge_index(repo, &mut snapshot.index, merged)?;
    snapshot.head_tree = Some(merged.result_tree);
    write_index_after_merge_checkout(repo, &mut snapshot.index, &mut lock)
}

/// Single index write for a merge checkout, with the same racy-entry policy as checkout's own
/// write: the clean-worktree guard already verified untouched entries and checkout just stat'ed
/// the entries it wrote, so re-hashing every same-second entry would only repeat that work.
///
/// `lock` is the `index.lock` taken before the checkout touched the work tree.
pub(crate) fn write_index_after_merge_checkout(
    repo: &Repository,
    index: &mut crate::index::Index,
    lock: &mut IndexLock,
) -> Result<()> {
    let index_path = lock.index_path.clone();
    index.skip_racy_smudge_on_write = true;
    let result = repo.write_index_with_lock(lock, index, true);
    index.skip_racy_smudge_on_write = false;
    result?;
    crate::index::advance_index_file_mtime_past_entries(index, &index_path, None)?;
    index.source_mtime = crate::index::index_file_mtime(&index_path);
    Ok(())
}

/// Index and stat refresh after a clean merge when the worktree was already updated (e.g. scoped merge checkout).
pub(crate) fn finalize_clean_tree_merge_index(
    repo: &Repository,
    index: &mut crate::index::Index,
    merged: &TreeMergeOutput,
) -> Result<()> {
    apply_tree_merge_output_to_worktree_index(repo, index, merged)?;
    if index.cache_tree.is_some() {
        index.cache_tree_root = Some(merged.result_tree);
    }
    Ok(())
}
