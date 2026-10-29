//! Apply a clean three-way tree merge to the worktree and index with one tree diff.
//!
//! Pick and revert already computed [`TreeMergeOutput`]; this module reuses that result for
//! untracked guards, worktree checkout, and sparse index updates instead of separate full-tree diffs.

use std::collections::{BTreeSet, HashSet};

use crate::diff::{diff_trees, diff_trees_with_index, DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::index::IndexLock;
use crate::merge_trees::{
    apply_tree_merge_output_to_index, apply_tree_merge_output_to_worktree_index, TreeMergeOutput,
};
use crate::objects::ObjectId;
use crate::porcelain::checkout::{
    checkout_tree_changes_with_index_deferred, checkout_tree_changes_with_index_opts,
    CheckoutApplyOptions, CheckoutIndexWrite,
};
use crate::porcelain::index_from_tree::refresh_stage0_index_stats_for_paths;
use crate::porcelain::switch::{prepare_tree_transition_allow_local, TreeTransitionPrepareError};
use crate::porcelain::worktree_guard::{
    collect_untracked_for_tree_changes, ensure_no_untracked_overwrite, load_worktree_snapshot,
    WorktreeSnapshot,
};
use crate::progress::{poll_cancel, Cancel, ProgressSink};
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;

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
/// Propagates diff, checkout, and index write failures, and [`Error::Cancelled`] when `cancel`
/// fires during the checkout (the index on disk is then unchanged).
pub(crate) fn apply_clean_tree_merge_to_worktree(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    apply_clean_tree_merge_to_worktree_with_snapshot(
        repo,
        from_tree,
        merged,
        &mut snapshot,
        progress,
        cancel,
    )
}

/// Like [`apply_clean_tree_merge_to_worktree`], but reuses a loaded worktree snapshot across steps.
pub(crate) fn apply_clean_tree_merge_to_worktree_with_snapshot(
    repo: &Repository,
    from_tree: &ObjectId,
    merged: &TreeMergeOutput,
    snapshot: &mut WorktreeSnapshot,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
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
        progress,
        cancel,
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

fn path_from_diff(entry: &DiffEntry) -> Option<&str> {
    entry.new_path.as_deref().or(entry.old_path.as_deref())
}

/// Checkout and index update for a conflicted three-way merge using one index snapshot.
///
/// `materialized` is the stage-0 tree layout before conflict-marker blobs are written
/// (from [`crate::merge_trees::materialized_merge_result_tree`]). Paths in `conflict_paths`
/// are left for marker writes; everything else follows the transition to `materialized`.
///
/// # Errors
///
/// Propagates transition planning, checkout, and index write failures, and [`Error::Cancelled`]
/// when `cancel` fires during checkout (the on-disk index is unchanged).
pub(crate) fn apply_conflict_tree_merge_to_worktree(
    repo: &Repository,
    head_tree: &ObjectId,
    merged: &TreeMergeOutput,
    materialized: &ObjectId,
    conflict_paths: &BTreeSet<Vec<u8>>,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let prepared =
        prepare_tree_transition_allow_local(repo, Some(head_tree), materialized, &HashSet::new())
            .map_err(|err| match err {
            TreeTransitionPrepareError::OverwritePaths(paths) => Error::WouldOverwrite {
                reason: crate::error::OverwriteReason::LocalChanges,
                paths,
                message: "local changes would be overwritten by merge".into(),
            },
            TreeTransitionPrepareError::Failed(err) => err,
        })?;

    let tree_changes = prepared.tree_changes();
    let preserve = prepared.preserve_worktree_paths();
    let mut filtered_tree_changes = Vec::with_capacity(tree_changes.len());
    for change in tree_changes {
        let Some(path) = path_from_diff(change) else {
            filtered_tree_changes.push(change.clone());
            continue;
        };
        if conflict_paths.contains(path.as_bytes()) || preserve.contains(path) {
            continue;
        }
        filtered_tree_changes.push(change.clone());
    }

    let overwrite_untracked = if filtered_tree_changes
        .iter()
        .any(|c| matches!(c.status, DiffStatus::Added | DiffStatus::Modified))
    {
        prepared.untracked_for_overwrite_check()
    } else {
        &[][..]
    };
    ensure_no_untracked_overwrite(overwrite_untracked, &filtered_tree_changes)?;

    let trust_wholesale = prepared.trust_wholesale_dir_removal();
    let wholesale_dirs = prepared.preverified_wholesale_dirs().clone();
    let wholesale_precomputed = prepared.wholesale_safety_precomputed();
    let mut lock = repo.lock_index()?;
    let mut index = prepared.into_index();
    let mut journal = checkout_tree_changes_with_index_deferred(
        repo,
        &filtered_tree_changes,
        &mut index,
        progress,
        cancel,
        CheckoutApplyOptions {
            parallel_prepare_blobs: true,
            trust_wholesale_dir_removal: trust_wholesale,
            preverified_wholesale_dirs: wholesale_dirs,
            wholesale_safety_precomputed: wholesale_precomputed,
            checkout_result_tree: Some(*materialized),
            ..CheckoutApplyOptions::default()
        },
        None,
        &lock,
    )?;

    let result = (|| -> Result<()> {
        apply_tree_merge_output_to_index(&mut index, merged);
        if index.has_unmerged_entries() {
            index.clear_cache_tree();
        }
        poll_cancel(progress, cancel)?;

        let rules = WorktreeRules::from_repository(repo, &index)?;
        for (path, oid) in &merged.conflict_content {
            let mode = index
                .get(path, 2)
                .or_else(|| index.get(path, 3))
                .map(|e| e.mode)
                .unwrap_or(0o100644);
            let rel = String::from_utf8_lossy(path);
            crate::porcelain::checkout::write_smudged_blob_to_worktree(
                repo,
                work_tree,
                &rules,
                rel.as_ref(),
                oid,
                mode,
            )?;
        }

        let mut stat_paths = BTreeSet::<String>::new();
        for change in &filtered_tree_changes {
            if let Some(path) = path_from_diff(change) {
                stat_paths.insert(path.to_owned());
            }
        }
        for path in merged.conflict_content.keys() {
            stat_paths.insert(String::from_utf8_lossy(path).into_owned());
        }
        let stat_ref: BTreeSet<&str> = stat_paths.iter().map(String::as_str).collect();
        refresh_stage0_index_stats_for_paths(repo, work_tree, &mut index, &stat_ref)?;

        write_index_after_merge_checkout(repo, &mut index, &mut lock)
    })();

    match result {
        Ok(()) => {
            journal.commit();
            Ok(())
        }
        Err(err) => journal.finish(repo, Err(err)),
    }
}
