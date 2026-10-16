//! Build stage-0 indexes from tree objects and refresh worktree stat caches.

use std::path::Path;

use crate::diff::refresh_index_stat_content_verified_with_rules;
use crate::error::Result;
use crate::index::{index_file_mtime, Index};
use crate::objects::ObjectId;
use crate::porcelain::merge::tree_to_index_entries;
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;

/// Build a sorted stage-0 index whose OIDs and modes match `tree_oid` (zeroed stat fields).
pub(crate) fn index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Index> {
    let entries = tree_to_index_entries(repo, tree_oid, "")?;
    let mut index = Index::new();
    for entry in entries {
        index.push_entry_unsorted(entry);
    }
    index.sort();
    Ok(index)
}

/// Refresh stage-0 stat fields from the worktree when blob content matches the index OID.
///
/// Tree-built indexes start with zeroed stat data; callers must run this before writing the
/// index when the worktree still matches the tree (mixed reset, stash push, merge abort).
pub(crate) fn refresh_stage0_index_stats_from_worktree(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
) -> Result<()> {
    if !index.entries.iter().any(|e| e.stage() == 0) {
        return Ok(());
    }
    let index_mtime = index_file_mtime(&repo.index_path());
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    let _ = refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        index,
        work_tree,
        index_mtime,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )?;
    Ok(())
}

/// Like [`index_from_tree`], then refresh stat caches from the worktree when a work tree exists.
pub(crate) fn index_from_tree_with_worktree_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<Index> {
    let mut index = index_from_tree(repo, tree_oid)?;
    if let Some(work_tree) = repo.work_tree() {
        refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    }
    Ok(index)
}
