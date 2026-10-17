//! Split one commit by pathspec: peel matching changes into a new parent commit.

use std::collections::HashSet;

use crate::diff::{diff_trees_with_renames, DiffEntry, DiffStatus};
use crate::index::{Index, IndexEntry};
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::pathspec::{matches_pathspec_list_with_context, PathspecMatchContext};
use crate::porcelain::index_from_tree::index_from_tree;
use crate::repo::Repository;
use crate::write_tree::write_tree_from_index;

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::reword::normalize_rewrite_message;
use super::types::{
    CommitEdit, HistoryEdits, NewCommit, Pathspec, RewriteOptions, RewritePlan, SplitOptions,
};

/// Plan a pathspec-limited split of `commit`: matching changes move into a new parent commit.
///
/// The target commit keeps its tree (and message unless [`SplitOptions::rest_message`] is set).
/// Descendants replay with parent-only updates when their trees are unchanged.
///
/// # Errors
///
/// Returns [`HistoryError::SplitSelectsNothing`], [`HistoryError::SplitSelectsEverything`],
/// [`HistoryError::InsertBeforeOnMerge`], [`HistoryError::EmptyMessage`], or errors from
/// [`plan_rewrite`].
pub fn split(
    repo: &Repository,
    commit: ObjectId,
    pathspec: Pathspec<'_>,
    split_message: String,
    opts: &SplitOptions,
    rewrite_opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    if pathspec.is_empty() {
        return Err(HistoryError::SplitSelectsNothing);
    }

    let split_message = normalize_rewrite_message(&split_message)?;
    let rest_message = opts
        .rest_message
        .as_ref()
        .map(|m| normalize_rewrite_message(m))
        .transpose()?;

    let raw = repo.odb().read(&commit).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(HistoryError::Planning {
            message: format!("expected commit, got {}", raw.kind.as_str()),
        });
    }
    let parsed = parse_commit(&raw.data).map_err(library_err)?;

    if parsed.parents.len() > 1 {
        return Err(HistoryError::InsertBeforeOnMerge { commit });
    }

    let parent_tree = if parsed.parents.is_empty() {
        repo.odb()
            .write(ObjectKind::Tree, &[])
            .map_err(library_err)?
    } else {
        let parent = parsed.parents[0];
        parse_commit(&repo.odb().read(&parent).map_err(library_err)?.data)
            .map_err(library_err)?
            .tree
    };

    let ctx = repo.pathspec_match_context();
    let split_tree = build_split_tree(repo, parent_tree, parsed.tree, pathspec, ctx)?;

    let mut edits = HistoryEdits::new();
    edits.insert(
        commit,
        CommitEdit::SplitBefore {
            insert: NewCommit {
                tree: split_tree,
                author: parsed.author.clone(),
                message: split_message,
            },
            rest_message,
        },
    );
    plan_rewrite(repo, &edits, rewrite_opts)
}

fn build_split_tree(
    repo: &Repository,
    parent_tree: ObjectId,
    commit_tree: ObjectId,
    pathspec: Pathspec<'_>,
    ctx: PathspecMatchContext,
) -> Result<ObjectId, HistoryError> {
    let odb = repo.odb();
    let diff = diff_trees_with_renames(odb, Some(&parent_tree), Some(&commit_tree), "", 50)
        .map_err(library_err)?;

    if diff.is_empty() {
        return Err(HistoryError::SplitSelectsNothing);
    }

    let mut selected = Vec::new();
    for entry in &diff {
        if diff_entry_matches_pathspec(entry, pathspec, ctx) {
            selected.push(entry.clone());
        }
    }

    if selected.is_empty() {
        return Err(HistoryError::SplitSelectsNothing);
    }
    if selected.len() == diff.len() {
        return Err(HistoryError::SplitSelectsEverything);
    }

    let mut index = index_from_tree(repo, &parent_tree).map_err(library_err)?;
    for entry in &selected {
        apply_diff_entry_to_index(&mut index, entry)?;
    }
    write_tree_from_index(odb, &index, "").map_err(library_err)
}

fn diff_entry_matches_pathspec(
    entry: &DiffEntry,
    pathspec: Pathspec<'_>,
    ctx: PathspecMatchContext,
) -> bool {
    let mut paths = HashSet::new();
    if let Some(old) = &entry.old_path {
        paths.insert(old.as_str());
    }
    if let Some(new) = &entry.new_path {
        paths.insert(new.as_str());
    }
    paths
        .into_iter()
        .any(|path| matches_pathspec_list_with_context(path, pathspec, ctx))
}

fn apply_diff_entry_to_index(index: &mut Index, entry: &DiffEntry) -> Result<(), HistoryError> {
    match entry.status {
        DiffStatus::Added | DiffStatus::Copied => {
            let path = entry
                .new_path
                .as_deref()
                .ok_or_else(|| planning_err("added diff entry missing new path"))?;
            let mode = mode_from_octal(&entry.new_mode)?;
            index.add_or_replace(index_entry(path, mode, entry.new_oid));
        }
        DiffStatus::Deleted => {
            let path = entry
                .old_path
                .as_deref()
                .ok_or_else(|| planning_err("deleted diff entry missing old path"))?;
            index.remove_paths([path.as_bytes()]);
        }
        DiffStatus::Modified | DiffStatus::TypeChanged => {
            let path = entry
                .new_path
                .as_deref()
                .or(entry.old_path.as_deref())
                .ok_or_else(|| planning_err("modified diff entry missing path"))?;
            let mode = mode_from_octal(&entry.new_mode)?;
            index.add_or_replace(index_entry(path, mode, entry.new_oid));
        }
        DiffStatus::Renamed => {
            if let Some(old) = &entry.old_path {
                index.remove_paths([old.as_bytes()]);
            }
            let path = entry
                .new_path
                .as_deref()
                .ok_or_else(|| planning_err("rename diff entry missing new path"))?;
            let mode = mode_from_octal(&entry.new_mode)?;
            index.add_or_replace(index_entry(path, mode, entry.new_oid));
        }
        DiffStatus::Unmerged => {
            return Err(planning_err("unexpected unmerged diff entry in split"));
        }
    }
    Ok(())
}

fn index_entry(path: &str, mode: u32, oid: ObjectId) -> IndexEntry {
    let path_bytes = path.as_bytes().to_vec();
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path_bytes.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path_bytes,
        base_index_pos: 0,
    }
}

fn mode_from_octal(raw: &str) -> Result<u32, HistoryError> {
    u32::from_str_radix(raw, 8).map_err(|e| HistoryError::Planning {
        message: format!("invalid file mode {raw:?}: {e}"),
    })
}

fn library_err(err: crate::error::Error) -> HistoryError {
    if let crate::error::Error::History(inner) = err {
        return inner;
    }
    HistoryError::Library(Box::new(err))
}

fn planning_err(message: impl Into<String>) -> HistoryError {
    HistoryError::Planning {
        message: message.into(),
    }
}
