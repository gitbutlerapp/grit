//! Apply staged index changes onto a historical commit and replay descendants.

use crate::apply::{apply_patch_to_index, IndexApplyOptions};
use crate::diff::diff_index_to_tree;
use crate::diff::hunk_id::DiffSide;
use crate::index::write_tree::write_tree_from_index;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use crate::porcelain::hunk_select::{resolve_selection, stage_hunks, HunkSelection};
use crate::porcelain::index_from_tree::index_from_tree;
use crate::repo::Repository;
use crate::state::resolve_head;

use super::apply::apply_fixup_rewrite;
use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::reword::normalize_rewrite_message;
use super::types::{AppliedRewrite, ApplyOptions};
use super::types::{CommitEdit, EmptyPolicy, HistoryEdits, RewriteOptions, RewritePlan};

/// Options for [`fixup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixupOptions {
    /// Replacement message for the target commit; `None` keeps the existing message.
    pub message: Option<String>,
    /// How to handle the target (and replayed descendants) when a commit becomes empty.
    pub empty: EmptyPolicy,
    /// Ref scope, committer, and replay empty policy (the `empty` field is overwritten from [`Self::empty`]).
    pub rewrite: RewriteOptions,
    /// When set, fixup uses `HEAD`'s tree plus these unstaged hunks (in-memory index) instead of the full staged index.
    pub selection: Option<HunkSelection>,
    /// Pathspec filter when resolving [`Self::selection`].
    pub pathspecs: Vec<String>,
}

/// Plan a fixup of `target`: merge staged index changes onto `target`'s tree and replay descendants.
///
/// When [`FixupOptions::selection`] is set, only the selected unstaged hunks are folded; the on-disk
/// index is unchanged until [`apply_fixup_hunk_selection`].
///
/// Writes new commit objects into the ODB; refs are updated only when the caller applies the plan.
///
/// # Errors
///
/// Returns [`HistoryError::BareRepository`], [`HistoryError::NothingStaged`], merge
/// [`HistoryError::Conflict`], empty-policy failures, and other planning errors.
pub fn fixup(
    repo: &Repository,
    target: ObjectId,
    opts: &FixupOptions,
) -> Result<RewritePlan, HistoryError> {
    if repo.is_bare() {
        return Err(HistoryError::BareRepository);
    }

    let head_oid = head_commit_oid(repo)?;
    let head_tree = commit_tree(repo, head_oid)?;
    let index_tree = if let Some(selection) = &opts.selection {
        index_tree_for_fixup_selection(repo, head_tree, selection, &opts.pathspecs)?
    } else {
        let index = repo.load_index().map_err(library_err)?;
        let staged =
            diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false).map_err(library_err)?;
        if staged.is_empty() {
            return Err(HistoryError::NothingStaged);
        }
        write_tree_from_index(repo.odb(), &index, "").map_err(library_err)?
    };
    let target_commit = load_commit(repo, target)?;
    let target_tree = target_commit.tree;

    record_fixup_merge();
    let merge = merge_trees_three_way(
        repo,
        head_tree,
        target_tree,
        index_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .map_err(library_err)?;

    if merge.index.has_unmerged_entries() || !merge.conflict_content.is_empty() {
        return Err(HistoryError::Conflict {
            commit: target,
            paths: conflict_paths(&merge),
        });
    }

    let merged_tree = merge.result_tree;
    let became_empty = fixup_target_became_empty(repo, &target_commit, merged_tree)?;

    let message = match &opts.message {
        Some(raw) => Some(normalize_rewrite_message(raw)?),
        None => None,
    };

    let mut edits = HistoryEdits::new();
    if became_empty {
        match opts.empty {
            EmptyPolicy::Abort => {
                return Err(HistoryError::FixupMakesTargetEmpty { commit: target });
            }
            EmptyPolicy::Drop => {
                edits.insert(target, CommitEdit::Drop);
            }
            EmptyPolicy::Keep => {
                edits.insert(
                    target,
                    CommitEdit::Rewrite {
                        tree: Some(merged_tree),
                        message,
                        author: None,
                        parents: None,
                    },
                );
            }
        }
    } else {
        edits.insert(
            target,
            CommitEdit::Rewrite {
                tree: Some(merged_tree),
                message,
                author: None,
                parents: None,
            },
        );
    }

    let mut rewrite_opts = opts.rewrite.clone();
    rewrite_opts.empty = opts.empty;
    plan_rewrite(repo, &edits, &rewrite_opts)
}

/// Repository-relative paths touched by a hunk selection (for [`apply_fixup_rewrite`]).
///
/// # Errors
///
/// Returns [`HistoryError::HunkSelect`] when ids or line ranges cannot be resolved.
pub fn fixup_selection_paths(
    repo: &Repository,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<Vec<String>, HistoryError> {
    let patches = resolve_selection(repo, DiffSide::Unstaged, selection, pathspecs)?;
    let mut paths: Vec<String> = patches
        .iter()
        .filter_map(|p| p.effective_path().map(str::to_owned))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Apply a fixup plan built with [`FixupOptions::selection`]: stage selected hunks, then run [`apply_fixup_rewrite`].
///
/// # Errors
///
/// Same as [`apply_fixup_rewrite`], plus hunk resolution and staging failures.
pub fn apply_fixup_hunk_selection(
    repo: &Repository,
    plan: &RewritePlan,
    apply_opts: &ApplyOptions,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<AppliedRewrite, HistoryError> {
    let paths = fixup_selection_paths(repo, selection, pathspecs)?;
    stage_hunks(repo, DiffSide::Unstaged, selection, pathspecs, false)?;
    apply_fixup_rewrite(repo, plan, apply_opts, &paths)
}

fn index_tree_for_fixup_selection(
    repo: &Repository,
    head_tree: ObjectId,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<ObjectId, HistoryError> {
    let patches = resolve_selection(repo, DiffSide::Unstaged, selection, pathspecs)?;
    if patches.is_empty() {
        return Err(HistoryError::NothingStaged);
    }
    let mut index = index_from_tree(repo, &head_tree).map_err(library_err)?;
    apply_patch_to_index(
        repo,
        &mut index,
        &patches,
        &IndexApplyOptions { dry_run: false },
    )
    .map_err(|e| HistoryError::Planning {
        message: e.to_string(),
    })?;
    write_tree_from_index(repo.odb(), &index, "").map_err(library_err)
}

fn library_err(err: crate::error::Error) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
    }
}

fn head_commit_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
    head.oid().copied().ok_or_else(|| HistoryError::Planning {
        message: "HEAD does not point at a commit".into(),
    })
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    let raw = repo.odb().read(&oid).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(HistoryError::Planning {
            message: format!("{oid} is not a commit object"),
        });
    }
    parse_commit(&raw.data).map_err(library_err)
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, HistoryError> {
    Ok(load_commit(repo, commit)?.tree)
}

fn empty_tree_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    repo.odb().write(ObjectKind::Tree, &[]).map_err(library_err)
}

fn first_parent_tree(repo: &Repository, commit: &CommitData) -> Result<ObjectId, HistoryError> {
    let empty = empty_tree_oid(repo)?;
    if commit.parents.is_empty() {
        return Ok(empty);
    }
    commit_tree(repo, commit.parents[0])
}

fn fixup_target_became_empty(
    repo: &Repository,
    target: &CommitData,
    merged_tree: ObjectId,
) -> Result<bool, HistoryError> {
    let parent_tree = first_parent_tree(repo, target)?;
    Ok(merged_tree == parent_tree)
}

fn conflict_paths(merge: &crate::merge_trees::TreeMergeOutput) -> Vec<String> {
    use std::collections::HashSet;
    let mut paths = HashSet::new();
    for key in merge.conflict_content.keys() {
        paths.insert(String::from_utf8_lossy(key).into_owned());
    }
    for entry in &merge.index.entries {
        if entry.stage() != 0 {
            paths.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    paths.into_iter().collect()
}

#[cfg(any(test, feature = "unstable-bench-helpers"))]
static FIXUP_MERGE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0); // hygiene: merge-call counter for tests and unstable-bench-helpers

fn record_fixup_merge() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    FIXUP_MERGE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Repository-relative paths that differ between the index and `HEAD`'s tree (stage 0).
///
/// # Errors
///
/// Returns [`HistoryError::BareRepository`] or planning failures when the index or `HEAD` cannot be read.
pub fn staged_fixup_paths(repo: &Repository) -> Result<Vec<String>, HistoryError> {
    if repo.is_bare() {
        return Err(HistoryError::BareRepository);
    }
    let head_oid = head_commit_oid(repo)?;
    let head_tree = commit_tree(repo, head_oid)?;
    let index = repo.load_index().map_err(library_err)?;
    let staged =
        diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false).map_err(library_err)?;
    let mut paths: Vec<String> = staged
        .into_iter()
        .filter_map(|e| e.new_path.or(e.old_path))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}
