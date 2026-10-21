//! Apply a clean three-way tree merge to the worktree and index with one tree diff.
//!
//! Pick and revert already computed [`TreeMergeOutput`]; this module reuses that result for
//! untracked guards, worktree checkout, and sparse index updates instead of separate full-tree diffs.

use crate::error::Result;
use crate::merge_trees::{apply_tree_merge_output_to_worktree_index, TreeMergeOutput};
use crate::objects::ObjectId;
use crate::porcelain::worktree_guard::load_worktree_snapshot;
use crate::repo::Repository;

/// Refresh the index after a clean merge checkout (worktree update is done separately).
///
/// Callers must run a scoped tree transition checkout (for example
/// [`crate::porcelain::merge::integrate_branch`]) before this function.
///
/// # Errors
///
/// Propagates index write failures.
pub(crate) fn apply_clean_tree_merge_to_worktree(
    repo: &Repository,
    _from_tree: &ObjectId,
    merged: &TreeMergeOutput,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    apply_tree_merge_output_to_worktree_index(repo, &mut snapshot.index, merged)?;
    if snapshot.index.cache_tree.is_some() {
        snapshot.index.cache_tree_root = Some(merged.result_tree);
    }
    repo.write_index(&mut snapshot.index)?;
    Ok(())
}
