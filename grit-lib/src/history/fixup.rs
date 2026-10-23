//! Apply staged index changes onto a historical commit and replay descendants.

use std::fs;

use super::absorb::remaining_index_blob_for_path;
use super::apply::apply_fixup_rewrite;
use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::reword::normalize_rewrite_message;
use super::types::{AppliedRewrite, ApplyOptions};
use super::types::{CommitEdit, EmptyPolicy, HistoryEdits, RewriteOptions, RewritePlan};
use crate::apply::{
    apply_patch_to_blob, apply_patch_to_index, BlobApplyOptions, FilePatch, Hunk, HunkLine,
    IndexApplyOptions,
};
use crate::diff::diff_index_to_tree;
use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide, HunkId};
use crate::diff::structured::{StructuredHunk, StructuredLineKind};
use crate::diff::{hunk_diff_bundle, DiffOptions};
use crate::index::write_tree::write_tree_from_index;
use crate::index::{Index, IndexEntry, MODE_REGULAR};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::porcelain::hunk_select::{resolve_selection, stage_hunks, HunkSelection};
use crate::porcelain::index_from_tree::index_from_tree;
use crate::repo::Repository;
use crate::state::resolve_head;

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
/// Pre-existing staged edits on the same path as a selected unstaged hunk are preserved in the
/// index and worktree after apply. On failure, the index is restored to its pre-call state.
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
    let snapshot = capture_hunk_fixup_apply_snapshot(repo, &paths, selection, pathspecs)?;
    if let Err(err) = stage_hunks(repo, DiffSide::Unstaged, selection, pathspecs, false) {
        return Err(err.into());
    }
    match apply_fixup_rewrite(repo, plan, apply_opts, &paths) {
        Ok(applied) => {
            restore_after_hunk_fixup_apply(repo, &snapshot)?;
            Ok(applied)
        }
        Err(err) => {
            restore_index_bytes(repo, &snapshot.index_bytes)?;
            Err(err)
        }
    }
}

#[derive(Debug, Clone)]
struct HunkFixupPathSnapshot {
    path: String,
    pre_index_blob: Vec<u8>,
    had_pre_staged: bool,
    /// Unstaged hunks on this path that are not part of the fixup selection.
    remaining_unstaged_hunks: Vec<StructuredHunk>,
}

#[derive(Debug, Clone)]
struct HunkFixupApplySnapshot {
    index_bytes: Vec<u8>,
    pre_head_tree: ObjectId,
    paths: Vec<HunkFixupPathSnapshot>,
}

fn capture_hunk_fixup_apply_snapshot(
    repo: &Repository,
    paths: &[String],
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<HunkFixupApplySnapshot, HistoryError> {
    let index_bytes = fs::read(repo.index_path()).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    let pre_head_tree = commit_tree(repo, head_commit_oid(repo)?)?;
    let index = repo.load_index().map_err(library_err)?;
    let mut path_states = Vec::with_capacity(paths.len());
    for path in paths {
        let pre_head_blob = blob_at_path(repo, pre_head_tree, path).unwrap_or_default();
        let pre_index_blob = index_blob_at_path(repo, &index, path);
        let had_pre_staged = pre_index_blob != pre_head_blob;
        let remaining_unstaged_hunks =
            remaining_unstaged_hunks_for_path(repo, path, selection, pathspecs)?;
        path_states.push(HunkFixupPathSnapshot {
            path: path.clone(),
            pre_index_blob,
            had_pre_staged,
            remaining_unstaged_hunks,
        });
    }
    Ok(HunkFixupApplySnapshot {
        index_bytes,
        pre_head_tree,
        paths: path_states,
    })
}

fn restore_index_bytes(repo: &Repository, bytes: &[u8]) -> Result<(), HistoryError> {
    fs::write(repo.index_path(), bytes).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })
}

fn remaining_unstaged_hunks_for_path(
    repo: &Repository,
    path: &str,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<Vec<StructuredHunk>, HistoryError> {
    let opts = DiffOptions::from_config(repo.config().map_err(library_err)?.as_ref());
    let bundle = hunk_diff_bundle(repo, pathspecs, opts).map_err(library_err)?;
    let Some(file) = bundle.unstaged.iter().find(|f| f.path == path) else {
        return Ok(Vec::new());
    };
    if file.binary {
        return Ok(Vec::new());
    }
    let peer = bundle
        .peer_hunks
        .get(path)
        .map(|h| h.as_slice())
        .unwrap_or(&[]);
    let ids = hunk_ids_with_duplicate_peers(DiffSide::Unstaged, path, &file.hunks, peer);
    let selected: std::collections::HashSet<&HunkId> = selection.ids.iter().collect();
    Ok(file
        .hunks
        .iter()
        .zip(ids.iter())
        .filter(|(_, id)| !selected.contains(id))
        .map(|(hunk, _)| hunk.clone())
        .collect())
}

fn restore_after_hunk_fixup_apply(
    repo: &Repository,
    snapshot: &HunkFixupApplySnapshot,
) -> Result<(), HistoryError> {
    let post_head_tree = commit_tree(repo, head_commit_oid(repo)?)?;
    let mut index = repo.load_index().map_err(library_err)?;
    let mut index_changed = false;
    let worktree = repo.work_tree();
    for path_state in &snapshot.paths {
        if !path_state.had_pre_staged {
            continue;
        }
        let new_index_blob = remaining_index_blob_for_path(
            repo,
            snapshot.pre_head_tree,
            post_head_tree,
            &path_state.pre_index_blob,
            &path_state.path,
            &[],
        )?;
        upsert_index_blob(repo, &mut index, &path_state.path, &new_index_blob)?;
        index_changed = true;
        if let Some(wt) = worktree.as_ref() {
            let new_worktree = if path_state.remaining_unstaged_hunks.is_empty() {
                new_index_blob.clone()
            } else {
                let patch = file_patch_from_structured_hunks(
                    &path_state.path,
                    &path_state.remaining_unstaged_hunks,
                );
                apply_patch_to_blob(&new_index_blob, &patch, &BlobApplyOptions::default()).map_err(
                    |e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    },
                )?
            };
            let abs = wt.join(&path_state.path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
            }
            fs::write(&abs, &new_worktree).map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
        }
    }
    if index_changed {
        index.sort();
        repo.write_index(&mut index)
            .map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
    }
    Ok(())
}

fn file_patch_from_structured_hunks(path: &str, hunks: &[StructuredHunk]) -> FilePatch {
    let apply_hunks: Vec<Hunk> = hunks
        .iter()
        .enumerate()
        .map(|(i, h)| structured_hunk_to_apply_hunk(h, i + 1))
        .collect();
    FilePatch {
        diff_old_path: Some(path.to_string()),
        diff_new_path: Some(path.to_string()),
        old_path: Some(path.to_string()),
        new_path: Some(path.to_string()),
        saw_old_header: true,
        saw_new_header: true,
        old_mode: None,
        new_mode: None,
        old_mode_line: None,
        new_mode_line: None,
        is_new: false,
        is_deleted: false,
        is_rename: false,
        is_copy: false,
        similarity_index: None,
        dissimilarity_index: None,
        old_oid: None,
        new_oid: None,
        binary_patch: None,
        is_binary: false,
        hunks: apply_hunks,
        ws_rule: 0,
        is_toplevel_relative: true,
    }
}

fn structured_hunk_to_apply_hunk(hunk: &StructuredHunk, first_body_line: usize) -> Hunk {
    let mut lines = Vec::new();
    for line in &hunk.lines {
        let text: String = line.segments.iter().map(|s| s.text.as_str()).collect();
        match line.kind {
            StructuredLineKind::Context => lines.push(HunkLine::Context(text)),
            StructuredLineKind::Add => lines.push(HunkLine::Add(text)),
            StructuredLineKind::Del => lines.push(HunkLine::Remove(text)),
        }
        if line.no_newline_at_eof {
            lines.push(HunkLine::NoNewline);
        }
    }
    Hunk {
        old_start: hunk.old_start,
        old_count: hunk.old_lines,
        new_start: hunk.new_start,
        new_count: hunk.new_lines,
        first_body_line,
        lines,
    }
}

fn index_blob_at_path(repo: &Repository, index: &Index, path: &str) -> Vec<u8> {
    index
        .get(path.as_bytes(), 0)
        .and_then(|entry| repo.odb().read(&entry.oid).ok().map(|raw| raw.data))
        .unwrap_or_default()
}

fn upsert_index_blob(
    repo: &Repository,
    index: &mut Index,
    path: &str,
    bytes: &[u8],
) -> Result<(), HistoryError> {
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, bytes)
        .map_err(library_err)?;
    let key = path.as_bytes();
    if let Some(entry) = index.get_mut(key, 0) {
        entry.oid = oid;
        entry.size = bytes.len() as u32;
    } else {
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: bytes.len() as u32,
            oid,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: key.to_vec(),
            base_index_pos: 0,
        });
    }
    Ok(())
}

fn blob_at_path(repo: &Repository, tree: ObjectId, path: &str) -> Result<Vec<u8>, HistoryError> {
    let mut current = tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (idx, part) in parts.iter().enumerate() {
        let raw = repo.odb().read(&current).map_err(library_err)?;
        let entries = parse_tree(&raw.data).map_err(library_err)?;
        let Some(entry) = entries
            .iter()
            .find(|e| e.name.as_slice() == part.as_bytes())
        else {
            return Err(HistoryError::Planning {
                message: format!("path '{path}' not found in tree"),
            });
        };
        if idx + 1 == parts.len() {
            let blob = repo.odb().read(&entry.oid).map_err(library_err)?;
            return Ok(blob.data);
        }
        current = entry.oid;
    }
    Err(HistoryError::Planning {
        message: format!("path '{path}' not found in tree"),
    })
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
