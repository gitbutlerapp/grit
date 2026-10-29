//! Apply a [`RewritePlan`] atomically: refs, then index and worktree.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::config::ConfigSet;
use crate::diff::{mode_from_metadata, DiffEntry, DiffStatus};
use crate::hooks::{run_post_checkout, run_post_rewrite, HookName, RewriteKind};
use crate::index::MODE_REGULAR;
use crate::lockfile::LockWait;
use crate::objects::ObjectId;
use crate::objects::ObjectKind;
use crate::porcelain::checkout::{
    materialize_gitlink_worktree, write_blob_to_worktree, LeadingDirCache,
};
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local, checkout_tree_transition_allow_local_with_preserved,
    prepare_tree_transition_allow_local, verify_tree_transition_allow_local,
    TreeTransitionPrepareError, TreeTransitionVerifyError,
};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::refs::prepare_with_lock_wait;
use crate::refs::read_symbolic_ref_for_repository;
use crate::refs::ref_update_flags;
use crate::refs::should_autocreate_reflog_with_config;
use crate::refs::store::RefUpdateFlags;
use crate::refs::store::{
    Expected, RawRef, RefStoreError, RefTransaction, RefUpdate, ReflogUpdate,
};
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};
use crate::worktree_rules::WorktreeRules;

use similar::{capture_diff_slices, Algorithm, DiffTag};

use super::absorb::{remaining_index_blob_for_path, staged_absorb_paths, AbsorbPlan};
use super::error::HistoryError;
use super::last::{write_history_last, HistoryLastRecord};
use super::types::{AppliedRewrite, ApplyOptions, PlannedRefUpdate, RewritePlan};
use super::undo_work::{
    capture_path_states, install_path_states, states_not_in_place, tree_path_states, PathState,
    UndoWork,
};

fn maybe_record_history_last(
    repo: &Repository,
    opts: &ApplyOptions,
    plan: &RewritePlan,
    work: Option<&UndoWork>,
) -> Result<(), HistoryError> {
    let Some(operation) = opts.record_operation.as_deref() else {
        return Ok(());
    };
    let record = HistoryLastRecord::from_applied_plan(operation, plan);
    write_history_last(repo, &record, work)
}

#[allow(clippy::too_many_arguments)]
fn complete_applied_rewrite(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    head: ObjectId,
    needs_worktree: bool,
    index_snapshot: Option<&IndexSnapshot>,
    worktree_snapshot: Option<&HashMap<String, WorktreePathSnapshot>>,
    work: Option<&UndoWork>,
) -> Result<AppliedRewrite, HistoryError> {
    if let Err(record_err) = maybe_record_history_last(repo, opts, plan, work) {
        if opts.record_operation.is_some() {
            if !plan.ref_updates.is_empty() {
                let _ = rollback_ref_updates(repo, &plan.ref_updates, opts);
            }
            if needs_worktree {
                let _ = restore_worktree_after_failed_apply(
                    repo,
                    plan.head.old_tree.as_ref(),
                    index_snapshot,
                    worktree_snapshot,
                );
            }
        }
        return Err(record_err);
    }
    let hooks_run = if opts.hooks.enabled() {
        run_rewrite_hooks(repo, plan)
    } else {
        Vec::new()
    };
    Ok(AppliedRewrite {
        ref_updates: plan.ref_updates.clone(),
        head,
        hooks_run,
    })
}

/// Run the hooks Git runs around a rebase once `plan` is applied, then `post-rewrite rebase`
/// with [`RewritePlan::rewritten_pairs`]. Returns the hooks that ran.
///
/// `post-checkout` runs in a work tree when `HEAD`'s commit changed. For a rebase
/// ([`HeadEffect::checkout_onto`](super::types::HeadEffect::checkout_onto) set) it is
/// `<old HEAD> <onto> 1`, the checkout Git performs before replaying. For other history edits it
/// is `<old HEAD> <new HEAD> 1`, and only when `HEAD`'s tree changed.
pub(crate) fn run_rewrite_hooks(repo: &Repository, plan: &RewritePlan) -> Vec<HookName> {
    let mut hooks_run = Vec::new();
    let tree_changed = head_tree_changes(&plan.head.old_tree, &plan.head.new_tree);
    let checkout = match (plan.head_change(), plan.head.checkout_onto) {
        (Some((old, _)), Some(onto)) => Some((old, onto)),
        (Some(change), None) if tree_changed => Some(change),
        _ => None,
    };
    if let Some((old, new)) = checkout.filter(|_| !repo.is_bare()) {
        if run_post_checkout(repo, Some(old), new, true) {
            hooks_run.push(HookName::PostCheckout);
        }
    }
    if run_post_rewrite(repo, RewriteKind::Rebase, &plan.rewritten_pairs()) {
        hooks_run.push(HookName::PostRewrite);
    }
    hooks_run
}

fn split_blob_lines(data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &byte) in data.iter().enumerate() {
        if byte == b'\n' {
            lines.push(data[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < data.len() {
        lines.push(data[start..].to_vec());
    }
    lines
}

fn join_blob_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    lines.iter().flat_map(|line| line.iter().copied()).collect()
}

fn map_old_index_lines_to_new(old_index: &[Vec<u8>], new_index: &[Vec<u8>]) -> Vec<Option<usize>> {
    let mut old_to_new = vec![None; old_index.len()];
    let ops = capture_diff_slices(Algorithm::Myers, old_index, new_index);
    for op in ops {
        let old_range = op.old_range();
        let new_range = op.new_range();
        match op.tag() {
            DiffTag::Equal => {
                for k in 0..old_range.len() {
                    old_to_new[old_range.start + k] = Some(new_range.start + k);
                }
            }
            DiffTag::Replace => {
                let paired = old_range.len().min(new_range.len());
                for k in 0..paired {
                    old_to_new[old_range.start + k] = Some(new_range.start + k);
                }
            }
            DiffTag::Delete | DiffTag::Insert => {}
        }
    }
    old_to_new
}

/// Reapply the unstaged worktree delta from `(old_index → old_worktree)` onto `new_index`.
pub(crate) fn apply_index_to_worktree_delta(
    old_index: &[u8],
    old_worktree: &[u8],
    new_index: &[u8],
) -> Vec<u8> {
    if old_index == old_worktree {
        return new_index.to_vec();
    }
    if old_index == new_index {
        return old_worktree.to_vec();
    }

    let old_index_lines = split_blob_lines(old_index);
    let old_worktree_lines = split_blob_lines(old_worktree);
    let new_index_lines = split_blob_lines(new_index);
    if old_index_lines.is_empty() && old_worktree_lines.is_empty() {
        return new_index.to_vec();
    }

    let old_to_new = map_old_index_lines_to_new(&old_index_lines, &new_index_lines);
    let unstaged_ops = capture_diff_slices(Algorithm::Myers, &old_index_lines, &old_worktree_lines);
    let mut out = Vec::new();
    let mut skip_new = vec![false; new_index_lines.len()];

    for op in unstaged_ops {
        match op.tag() {
            DiffTag::Equal => {
                for (k, old_i) in op.old_range().enumerate() {
                    let worktree_i = op.new_range().start + k;
                    let _ = worktree_i;
                    if let Some(new_i) = old_to_new.get(old_i).copied().flatten() {
                        out.push(new_index_lines[new_i].clone());
                        skip_new[new_i] = true;
                    } else if old_i < old_index_lines.len() {
                        out.push(old_index_lines[old_i].clone());
                    }
                }
            }
            DiffTag::Delete => {
                for old_i in op.old_range() {
                    if let Some(new_i) = old_to_new.get(old_i).copied().flatten() {
                        skip_new[new_i] = true;
                    }
                }
            }
            DiffTag::Insert => {
                for worktree_i in op.new_range() {
                    if worktree_i < old_worktree_lines.len() {
                        out.push(old_worktree_lines[worktree_i].clone());
                    }
                }
            }
            DiffTag::Replace => {
                for old_i in op.old_range() {
                    if let Some(new_i) = old_to_new.get(old_i).copied().flatten() {
                        skip_new[new_i] = true;
                    }
                }
                for worktree_i in op.new_range() {
                    if worktree_i < old_worktree_lines.len() {
                        out.push(old_worktree_lines[worktree_i].clone());
                    }
                }
            }
        }
    }

    for (new_i, line) in new_index_lines.iter().enumerate() {
        if !skip_new[new_i] {
            out.push(line.clone());
        }
    }

    join_blob_lines(&out)
}

/// Apply an absorb [`AbsorbPlan`] and leave unassigned hunks staged.
///
/// # Errors
///
/// Same as [`apply_fixup_rewrite`].
pub fn apply_absorb_rewrite(
    repo: &Repository,
    absorb: &AbsorbPlan,
    opts: &ApplyOptions,
) -> Result<AppliedRewrite, HistoryError> {
    let staged_paths = staged_absorb_paths(repo)?;
    let prestate = if !repo.is_bare() && !staged_paths.is_empty() {
        Some(FixupApplyPrestate::capture(repo, &staged_paths)?)
    } else {
        None
    };
    let consumed = consumed_states_for_undo(repo, opts, &staged_paths)?;
    apply_absorb_refs_and_worktree(
        repo,
        absorb,
        opts,
        &staged_paths,
        prestate.as_ref(),
        consumed,
    )
    .or_else(|err| {
        if let Some(pre) = prestate {
            pre.restore(repo)?;
        }
        Err(err)
    })
}

/// Pre-command state of `paths` for the undo sidecar, captured only when the apply is recorded.
fn consumed_states_for_undo(
    repo: &Repository,
    opts: &ApplyOptions,
    paths: &[String],
) -> Result<Option<Vec<PathState>>, HistoryError> {
    if opts.record_operation.is_none() {
        return Ok(None);
    }
    capture_path_states(repo, paths).map(Some)
}

fn undo_work_after_command(
    repo: &Repository,
    consumed: Option<Vec<PathState>>,
) -> Result<Option<UndoWork>, HistoryError> {
    consumed
        .map(|states| UndoWork::after_command(repo, states))
        .transpose()
}

fn apply_absorb_refs_and_worktree(
    repo: &Repository,
    absorb: &AbsorbPlan,
    opts: &ApplyOptions,
    staged_paths: &[String],
    prestate: Option<&FixupApplyPrestate>,
    consumed: Option<Vec<PathState>>,
) -> Result<AppliedRewrite, HistoryError> {
    let plan = &absorb.plan;
    let (old_tree, new_tree) = head_tree_transition(plan);
    let needs_worktree = !repo.is_bare() && head_tree_changes(&old_tree, &new_tree);
    let old_head_commit = if needs_worktree {
        Some(repo.head_commit()?)
    } else {
        None
    };
    run_planned_rewrite_apply(
        repo,
        plan,
        opts,
        rewrite_journal::RewriteApplyKind::Absorb,
        |repo, old_tree, new_tree| {
            let old_head_commit = old_head_commit.ok_or_else(|| HistoryError::WorktreeApply {
                message: "missing old HEAD commit for absorb apply".into(),
            })?;
            apply_absorb_worktree_after_refs(
                repo,
                old_tree,
                new_tree,
                old_head_commit,
                absorb,
                staged_paths,
                prestate,
            )?;
            undo_work_after_command(repo, consumed)
        },
    )
    .inspect_err(|_| {
        if let Some(pre) = prestate {
            let _ = pre.restore(repo);
        }
    })
}

fn apply_absorb_worktree_after_refs(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    old_head_commit: ObjectId,
    absorb: &AbsorbPlan,
    staged_paths: &[String],
    prestate: Option<&FixupApplyPrestate>,
) -> Result<(), HistoryError> {
    if !staged_paths.is_empty() {
        clear_fixup_staging_after_apply(repo, old_head_commit, staged_paths)?;
        let old_head_tree = commit_tree_for_apply(repo, old_head_commit)?;
        align_worktree_to_head_for_paths(repo, old_head_tree, staged_paths)?;
    }
    checkout_tree_transition_allow_local(repo, from_tree, to_tree).map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })?;
    if staged_paths.is_empty() {
        return Ok(());
    }
    let new_head = repo.head_commit()?;
    let new_head_tree = commit_tree_for_apply(repo, new_head)?;
    let old_head_tree = commit_tree_for_apply(repo, old_head_commit)?;
    let mut index = repo.load_index().map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    for path in staged_paths {
        let pre_index_blob = prestate
            .and_then(|p| {
                p.paths
                    .iter()
                    .find(|entry| entry.path == *path)
                    .and_then(|entry| entry.index_blob.clone())
            })
            .unwrap_or_default();
        let blob = remaining_index_blob_for_path(
            repo,
            old_head_tree,
            new_head_tree,
            &pre_index_blob,
            path,
            &absorb.assignments,
        )?;
        let oid =
            repo.odb()
                .write(ObjectKind::Blob, &blob)
                .map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
        if let Some(entry) = index.get_mut(path.as_bytes(), 0) {
            entry.oid = oid;
            entry.size = blob.len() as u32;
        }
    }
    index.sort();
    repo.write_index(&mut index)
        .map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
    if let Some(pre) = prestate {
        pre.reapply_unstaged_worktree_deltas(repo)?;
    }
    Ok(())
}

/// Apply a fixup [`RewritePlan`] and align the index with the new `HEAD` on paths that were staged.
///
/// # Errors
///
/// Same as [`apply_rewrite`], plus index update failures after a successful ref move.
pub fn apply_fixup_rewrite(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    staged_paths: &[String],
) -> Result<AppliedRewrite, HistoryError> {
    let consumed = consumed_states_for_undo(repo, opts, staged_paths)?;
    apply_fixup_rewrite_then(repo, plan, opts, staged_paths, consumed, |_| Ok(()))
}

/// [`apply_fixup_rewrite`] with the undo sidecar's pre-command states supplied by the caller and
/// `finish` run on the work tree before the rewrite is recorded.
///
/// `consumed` is `None` when the apply is not recorded. A `finish` failure rolls the apply back.
pub(crate) fn apply_fixup_rewrite_then(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    staged_paths: &[String],
    consumed: Option<Vec<PathState>>,
    finish: impl FnOnce(&Repository) -> Result<(), HistoryError>,
) -> Result<AppliedRewrite, HistoryError> {
    let prestate = if !repo.is_bare() && !staged_paths.is_empty() {
        Some(FixupApplyPrestate::capture(repo, staged_paths)?)
    } else {
        None
    };
    apply_fixup_refs_and_worktree(
        repo,
        plan,
        opts,
        staged_paths,
        prestate.as_ref(),
        consumed,
        finish,
    )
    .or_else(|err| {
        if let Some(pre) = prestate {
            pre.restore(repo)?;
        }
        Err(err)
    })
}

struct StagedPathPrestate {
    path: String,
    worktree: Option<Vec<u8>>,
    index_blob: Option<Vec<u8>>,
    /// Worktree file content and mode matched the staged index entry at capture time.
    worktree_matches_staged: bool,
}

fn worktree_matches_staged_index(
    worktree_path: &std::path::Path,
    index_blob: &[u8],
    index_mode: u32,
) -> Result<bool, HistoryError> {
    if index_mode != MODE_REGULAR && index_mode != crate::index::MODE_EXECUTABLE {
        return Ok(false);
    }
    if !worktree_path.is_file() {
        return Ok(false);
    }
    let meta = fs::metadata(worktree_path).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    if mode_from_metadata(&meta) != index_mode {
        return Ok(false);
    }
    let worktree_bytes = fs::read(worktree_path).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    Ok(worktree_bytes == index_blob)
}

struct FixupApplyPrestate {
    index_bytes: Vec<u8>,
    paths: Vec<StagedPathPrestate>,
}

impl FixupApplyPrestate {
    fn capture(repo: &Repository, staged_paths: &[String]) -> Result<Self, HistoryError> {
        let index_bytes = fs::read(repo.index_path()).map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
        let index = repo.load_index().map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
        let worktree = repo.work_tree();
        let mut paths = Vec::with_capacity(staged_paths.len());
        for path in staged_paths {
            let index_entry = index.get(path.as_bytes(), 0);
            let index_blob =
                index_entry.and_then(|entry| repo.odb().read(&entry.oid).ok().map(|raw| raw.data));
            let worktree_bytes = if let Some(wt) = worktree {
                let abs = wt.join(path);
                if abs.is_file() {
                    Some(fs::read(&abs).map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?)
                } else {
                    None
                }
            } else {
                None
            };
            let worktree_matches_staged = match (worktree, index_entry, index_blob.as_deref()) {
                (Some(wt), Some(entry), Some(blob)) => {
                    worktree_matches_staged_index(&wt.join(path), blob, entry.mode)?
                }
                _ => false,
            };
            paths.push(StagedPathPrestate {
                path: path.clone(),
                worktree: worktree_bytes,
                index_blob,
                worktree_matches_staged,
            });
        }
        Ok(Self { index_bytes, paths })
    }

    fn restore(&self, repo: &Repository) -> Result<(), HistoryError> {
        fs::write(repo.index_path(), &self.index_bytes).map_err(|e| {
            HistoryError::WorktreeApply {
                message: e.to_string(),
            }
        })?;
        let Some(worktree) = repo.work_tree() else {
            return Ok(());
        };
        for path in &self.paths {
            let abs = worktree.join(&path.path);
            match &path.worktree {
                Some(bytes) => {
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent).map_err(|e| HistoryError::WorktreeApply {
                            message: e.to_string(),
                        })?;
                    }
                    fs::write(&abs, bytes).map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
                }
                None => {
                    if abs.is_file() {
                        fs::remove_file(&abs).map_err(|e| HistoryError::WorktreeApply {
                            message: e.to_string(),
                        })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn restore_worktrees_not_matching_staged(&self, repo: &Repository) -> Result<(), HistoryError> {
        let Some(worktree) = repo.work_tree() else {
            return Ok(());
        };
        for path in &self.paths {
            if path.worktree_matches_staged {
                continue;
            }
            let abs = worktree.join(&path.path);
            match &path.worktree {
                Some(bytes) => {
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent).map_err(|e| HistoryError::WorktreeApply {
                            message: e.to_string(),
                        })?;
                    }
                    fs::write(&abs, bytes).map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
                }
                None => {
                    if abs.is_file() {
                        fs::remove_file(&abs).map_err(|e| HistoryError::WorktreeApply {
                            message: e.to_string(),
                        })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn force_preserve_worktree_paths(&self, paths: &[String]) -> HashSet<String> {
        paths
            .iter()
            .filter(|path| {
                self.paths
                    .iter()
                    .find(|p| p.path == **path)
                    .is_some_and(|p| !p.worktree_matches_staged)
            })
            .cloned()
            .collect()
    }

    fn filter_paths_with_matching_worktree(&self, paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .filter(|path| {
                self.paths
                    .iter()
                    .find(|p| p.path == **path)
                    .is_some_and(|p| p.worktree_matches_staged)
            })
            .cloned()
            .collect()
    }

    fn sync_index_to_captured_worktree_for_nonmatching(
        &self,
        repo: &Repository,
        staged_paths: &[String],
    ) -> Result<(), HistoryError> {
        let Some(worktree) = repo.work_tree() else {
            return Ok(());
        };
        let mut index = repo.load_index().map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
        let mut changed = false;
        for path in staged_paths {
            let Some(pre) = self.paths.iter().find(|p| p.path == *path) else {
                continue;
            };
            if pre.worktree_matches_staged {
                continue;
            }
            let key = path.as_bytes();
            match &pre.worktree {
                None => {
                    if index.get(key, 0).is_some() {
                        index.remove_path_all_stages(key);
                        changed = true;
                    }
                }
                Some(bytes) => {
                    let oid = repo
                        .odb()
                        .write(crate::objects::ObjectKind::Blob, bytes)
                        .map_err(|e| HistoryError::WorktreeApply {
                            message: e.to_string(),
                        })?;
                    let abs = worktree.join(path);
                    let mode = if abs.is_file() {
                        mode_from_metadata(&fs::metadata(&abs).map_err(|e| {
                            HistoryError::WorktreeApply {
                                message: e.to_string(),
                            }
                        })?)
                    } else {
                        MODE_REGULAR
                    };
                    if let Some(entry) = index.get_mut(key, 0) {
                        entry.oid = oid;
                        entry.mode = mode;
                        entry.size = bytes.len() as u32;
                    } else {
                        index.add_or_replace(crate::index::IndexEntry {
                            ctime_sec: 0,
                            ctime_nsec: 0,
                            mtime_sec: 0,
                            mtime_nsec: 0,
                            dev: 0,
                            ino: 0,
                            mode,
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
                    changed = true;
                }
            }
        }
        if changed {
            index.sort();
            repo.write_index(&mut index)
                .map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }

    fn reapply_unstaged_worktree_deltas(&self, repo: &Repository) -> Result<(), HistoryError> {
        let Some(worktree) = repo.work_tree() else {
            return Ok(());
        };
        let index = repo.load_index().map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
        for path in &self.paths {
            if !path.worktree_matches_staged {
                continue;
            }
            let Some(pre_worktree) = &path.worktree else {
                continue;
            };
            let Some(pre_index_blob) = &path.index_blob else {
                continue;
            };
            if pre_worktree == pre_index_blob {
                continue;
            }
            let Some(entry) = index.get(path.path.as_bytes(), 0) else {
                continue;
            };
            let head_blob =
                repo.odb()
                    .read(&entry.oid)
                    .map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
            let target =
                apply_index_to_worktree_delta(pre_index_blob, pre_worktree, &head_blob.data);
            let abs = worktree.join(&path.path);
            fs::write(&abs, &target).map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
        }
        Ok(())
    }
}

fn apply_fixup_refs_and_worktree(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    staged_paths: &[String],
    prestate: Option<&FixupApplyPrestate>,
    consumed: Option<Vec<PathState>>,
    finish: impl FnOnce(&Repository) -> Result<(), HistoryError>,
) -> Result<AppliedRewrite, HistoryError> {
    let (old_tree, new_tree) = head_tree_transition(plan);
    let needs_worktree = !repo.is_bare() && head_tree_changes(&old_tree, &new_tree);
    let old_head_commit = if needs_worktree {
        Some(repo.head_commit()?)
    } else {
        None
    };
    run_planned_rewrite_apply(
        repo,
        plan,
        opts,
        rewrite_journal::RewriteApplyKind::Fixup,
        |repo, old_tree, new_tree| {
            let old_head_commit = old_head_commit.ok_or_else(|| HistoryError::WorktreeApply {
                message: "missing old HEAD commit for fixup apply".into(),
            })?;
            apply_fixup_worktree_after_refs(
                repo,
                old_tree,
                new_tree,
                old_head_commit,
                staged_paths,
                prestate,
            )?;
            finish(repo)?;
            undo_work_after_command(repo, consumed)
        },
    )
    .inspect_err(|_| {
        if let Some(pre) = prestate {
            let _ = pre.restore(repo);
        }
    })
}

fn apply_fixup_worktree_after_refs(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    old_head_commit: ObjectId,
    staged_paths: &[String],
    prestate: Option<&FixupApplyPrestate>,
) -> Result<(), HistoryError> {
    if !staged_paths.is_empty() {
        clear_fixup_staging_after_apply(repo, old_head_commit, staged_paths)?;
        let old_head_tree = commit_tree_for_apply(repo, old_head_commit)?;
        let align_paths = prestate
            .map(|pre| pre.filter_paths_with_matching_worktree(staged_paths))
            .unwrap_or_else(|| staged_paths.to_vec());
        if !align_paths.is_empty() {
            align_worktree_to_head_for_paths(repo, old_head_tree, &align_paths)?;
        }
        if let Some(pre) = prestate {
            pre.sync_index_to_captured_worktree_for_nonmatching(repo, staged_paths)?;
        }
    }
    let force_preserve = prestate
        .map(|pre| pre.force_preserve_worktree_paths(staged_paths))
        .unwrap_or_default();
    checkout_tree_transition_allow_local_with_preserved(
        repo,
        from_tree,
        to_tree,
        &force_preserve,
        None,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )
    .map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    if !staged_paths.is_empty() {
        let head = repo.head_commit()?;
        clear_fixup_staging_after_apply(repo, head, staged_paths)?;
        if let Some(pre) = prestate {
            pre.restore_worktrees_not_matching_staged(repo)?;
            pre.reapply_unstaged_worktree_deltas(repo)?;
        }
    }
    Ok(())
}

fn align_worktree_to_head_for_paths(
    repo: &Repository,
    head_tree: ObjectId,
    paths: &[String],
) -> Result<(), HistoryError> {
    let Some(worktree) = repo.work_tree() else {
        return Ok(());
    };
    for path in paths {
        let abs = worktree.join(path);
        match tree_entry_at_path(repo, head_tree, path)? {
            Some((oid, _mode)) => {
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
                }
                let blob = repo
                    .odb()
                    .read(&oid)
                    .map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
                fs::write(&abs, &blob.data).map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
            }
            None => {
                if abs.is_file() {
                    fs::remove_file(&abs).map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
                }
            }
        }
    }
    Ok(())
}

fn clear_fixup_staging_after_apply(
    repo: &Repository,
    head_commit: ObjectId,
    staged_paths: &[String],
) -> Result<(), HistoryError> {
    let head_tree = commit_tree_for_apply(repo, head_commit)?;
    let mut index = repo.load_index().map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    for path in staged_paths {
        let key = path.as_bytes();
        match tree_entry_at_path(repo, head_tree, path)? {
            Some((oid, mode)) => {
                if let Some(entry) = index.get_mut(key, 0) {
                    entry.oid = oid;
                    entry.mode = mode;
                } else {
                    // A gitlink names a commit in the submodule's own object store.
                    let size = if mode == crate::index::MODE_GITLINK {
                        0
                    } else {
                        repo.odb()
                            .read(&oid)
                            .map_err(|e| HistoryError::WorktreeApply {
                                message: e.to_string(),
                            })?
                            .data
                            .len() as u32
                    };
                    index.add_or_replace(crate::index::IndexEntry {
                        ctime_sec: 0,
                        ctime_nsec: 0,
                        mtime_sec: 0,
                        mtime_nsec: 0,
                        dev: 0,
                        ino: 0,
                        mode,
                        uid: 0,
                        gid: 0,
                        size,
                        oid,
                        flags: path.len().min(0xfff) as u16,
                        flags_extended: None,
                        path: key.to_vec(),
                        base_index_pos: 0,
                    });
                }
            }
            None => {
                index.remove_path_all_stages(key);
            }
        }
    }
    index.sort();
    repo.write_index(&mut index)
        .map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })
}

fn tree_entry_at_path(
    repo: &Repository,
    tree: ObjectId,
    path: &str,
) -> Result<Option<(ObjectId, u32)>, HistoryError> {
    let entry = repo
        .odb()
        .tree_entry_at_path(&tree, path.as_bytes())
        .map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
    Ok(entry.map(|e| (e.oid, e.mode)))
}

pub(crate) fn commit_tree_for_apply(
    repo: &Repository,
    commit: ObjectId,
) -> Result<ObjectId, HistoryError> {
    repo.odb()
        .commit_tree(&commit)
        .map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })
}

/// Runs the same worktree safety checks as [`apply_rewrite`] without moving refs.
///
/// # Errors
///
/// Returns [`HistoryError::LocalChangesWouldBeOverwritten`] or [`HistoryError::WorktreeApply`].
pub fn verify_rewrite_plan(repo: &Repository, plan: &RewritePlan) -> Result<(), HistoryError> {
    let (old_tree, new_tree) = head_tree_transition(plan);
    if repo.is_bare() {
        return Ok(());
    }
    if let (Some(old), Some(new)) = (old_tree.as_ref(), new_tree.as_ref()) {
        verify_worktree_ready_for_head_tree_change(repo, Some(old), new)?;
    }
    Ok(())
}

/// Returns [`HistoryError::LocalChangesWouldBeOverwritten`], [`HistoryError::HeadMovedSincePlan`],
/// [`HistoryError::RefCasMismatch`], [`HistoryError::RefApply`], or [`HistoryError::WorktreeApply`].
pub fn apply_rewrite(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<AppliedRewrite, HistoryError> {
    verify_live_head_matches_plan(repo, &plan.head)?;
    let (old_tree, new_tree) = head_tree_transition(plan);
    if let (Some(old), Some(new)) = (old_tree.as_ref(), new_tree.as_ref()) {
        if !repo.is_bare() {
            verify_worktree_ready_for_head_tree_change(repo, Some(old), new)?;
        }
    }
    run_planned_rewrite_apply(
        repo,
        plan,
        opts,
        rewrite_journal::RewriteApplyKind::Standard,
        |repo, old_tree, new_tree| {
            apply_head_tree_to_worktree(repo, old_tree, new_tree, None, progress, cancel)?;
            Ok(None)
        },
    )
}

/// Work tree checks for undoing a rewrite whose sidecar restores `work`'s paths.
///
/// Every recorded path must still be as the rewrite left it. Those paths are then not local
/// changes for the inverse checkout: [`apply_undo_rewrite`] resets them before checking out.
/// Returns the live state of the recorded paths (empty without `work`).
pub(crate) fn verify_undo_worktree(
    repo: &Repository,
    plan: &RewritePlan,
    work: Option<&UndoWork>,
) -> Result<Vec<PathState>, HistoryError> {
    let Some(work) = work.filter(|_| !repo.is_bare()) else {
        verify_rewrite_plan(repo, plan)?;
        return Ok(Vec::new());
    };
    let live = work.unchanged_live_states(repo)?;
    let recorded = work.paths();
    match verify_rewrite_plan(repo, plan) {
        Err(HistoryError::LocalChangesWouldBeOverwritten { paths }) => {
            let paths: Vec<String> = paths
                .into_iter()
                .filter(|p| !recorded.contains(p))
                .collect();
            if !paths.is_empty() {
                return Err(HistoryError::LocalChangesWouldBeOverwritten { paths });
            }
        }
        other => other?,
    }
    Ok(live)
}

/// Apply an undo `plan`, then install the index and work tree states recorded in `work`.
///
/// The recorded paths are reset to `HEAD`'s tree before the inverse checkout, and their state
/// just before the undo becomes the new record's sidecar, so undoing again redoes the rewrite
/// with the same paths put back.
pub(crate) fn apply_undo_rewrite(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    work: Option<&UndoWork>,
) -> Result<AppliedRewrite, HistoryError> {
    verify_live_head_matches_plan(repo, &plan.head)?;
    let before = verify_undo_worktree(repo, plan, work)?;
    let paths = work.map(UndoWork::paths).unwrap_or_default();
    run_planned_rewrite_apply(
        repo,
        plan,
        opts,
        rewrite_journal::RewriteApplyKind::Standard,
        |repo, old_tree, new_tree| {
            let checkout = |repo: &Repository| {
                apply_head_tree_to_worktree(
                    repo,
                    old_tree,
                    new_tree,
                    None,
                    &mut crate::progress::NullProgress,
                    &crate::progress::NeverCancel,
                )
            };
            let Some(work) = work else {
                checkout(repo)?;
                return Ok(None);
            };
            let restore = work.restore_states();
            let installed = (|| {
                if let Some(from) = old_tree {
                    let reset =
                        states_not_in_place(tree_path_states(repo, *from, &paths)?, &before);
                    if !reset.is_empty() {
                        install_path_states(repo, &reset)?;
                    }
                }
                checkout(repo)?;
                install_path_states(repo, &restore)
            })();
            if let Err(err) = installed {
                let _ = install_path_states(repo, &before);
                return Err(err);
            }
            UndoWork::pair(repo.odb().hash_algo(), before, &restore).map(Some)
        },
    )
}

/// Move refs, then run `apply_worktree` when `HEAD`'s tree changes and record the rewrite.
///
/// `apply_worktree` returns the index and work tree state an undo must put back, stored next to
/// the undo record; `None` records refs only.
fn run_planned_rewrite_apply<F>(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    apply_kind: rewrite_journal::RewriteApplyKind,
    apply_worktree: F,
) -> Result<AppliedRewrite, HistoryError>
where
    F: FnOnce(&Repository, Option<&ObjectId>, &ObjectId) -> Result<Option<UndoWork>, HistoryError>,
{
    let mut work = None;
    let (old_tree, new_tree) = head_tree_transition(plan);
    let needs_worktree = !repo.is_bare() && head_tree_changes(&old_tree, &new_tree);
    let new_head_commit = head_commit_after(repo, &plan.ref_updates)?;

    let index_snapshot = if needs_worktree {
        Some(capture_index_snapshot(repo)?)
    } else {
        None
    };
    let worktree_snapshot = if needs_worktree {
        match (old_tree.as_ref(), new_tree.as_ref()) {
            (Some(old), Some(new)) => Some(snapshot_worktree_for_tree_transition(repo, old, new)?),
            _ => None,
        }
    } else {
        None
    };

    let index_lock = needs_worktree
        .then(|| repo.begin_index_update())
        .transpose()
        .map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;

    if needs_worktree {
        let index_before = index_snapshot
            .as_ref()
            .ok_or_else(|| HistoryError::WorktreeApply {
                message: "missing index snapshot for rewrite apply journal".into(),
            })?;
        let worktree_snap =
            worktree_snapshot
                .as_ref()
                .ok_or_else(|| HistoryError::WorktreeApply {
                    message: "missing worktree snapshot for rewrite apply journal".into(),
                })?;
        rewrite_journal::write_rewrite_apply_journal(
            repo,
            plan,
            opts,
            apply_kind,
            index_before,
            worktree_snap,
        )?;
    }

    if !plan.ref_updates.is_empty() {
        commit_planned_ref_updates(repo, &plan.ref_updates, opts)?;
    }

    drop(index_lock);

    if needs_worktree {
        if crate::repo::test_inject_rewrite_apply_abort_after_refs() {
            return Err(HistoryError::WorktreeApply {
                message: "injected abort after rewrite ref updates".into(),
            });
        }
        let (Some(old), Some(new)) = (old_tree, new_tree) else {
            let _ = rewrite_journal::clear_rewrite_apply_journal(repo);
            return Err(HistoryError::WorktreeApply {
                message: "missing head tree for worktree apply".into(),
            });
        };
        match apply_worktree(repo, Some(&old), &new) {
            Ok(recorded) => work = recorded,
            Err(checkout_err) => {
                let ref_rollback = rollback_ref_updates(repo, &plan.ref_updates, opts).err();
                let worktree_restore = restore_worktree_after_failed_apply(
                    repo,
                    Some(&old),
                    index_snapshot.as_ref(),
                    worktree_snapshot.as_ref(),
                )
                .err();
                if ref_rollback.is_none() && worktree_restore.is_none() {
                    let _ = rewrite_journal::clear_rewrite_apply_journal(repo);
                }
                return Err(apply_failure_after_ref_update(
                    checkout_err,
                    ref_rollback,
                    worktree_restore,
                ));
            }
        }
    }

    let applied = complete_applied_rewrite(
        repo,
        plan,
        opts,
        new_head_commit,
        needs_worktree,
        index_snapshot.as_ref(),
        worktree_snapshot.as_ref(),
        work.as_ref(),
    );
    if applied.is_ok() {
        let _ = rewrite_journal::clear_rewrite_apply_journal(repo);
    }
    applied
}

fn verify_live_head_matches_plan(
    repo: &Repository,
    head: &super::types::HeadEffect,
) -> Result<(), HistoryError> {
    let Some(planned_commit) = head.old_commit else {
        return Ok(());
    };
    let live = resolve_head_for_repository(repo).map_err(|e| HistoryError::RefApply {
        message: e.to_string(),
    })?;
    let mismatch = || HistoryError::HeadMovedSincePlan {
        planned_ref: head.head_ref.clone(),
        planned_commit,
    };
    match (&head.head_ref, live) {
        (Some(planned_ref), HeadState::Branch { refname, oid, .. }) => {
            if planned_ref != &refname {
                return Err(mismatch());
            }
            if oid.as_ref() != Some(&planned_commit) {
                return Err(mismatch());
            }
        }
        (None, HeadState::Detached { oid }) => {
            if oid != planned_commit {
                return Err(mismatch());
            }
        }
        (Some(_), HeadState::Detached { .. }) | (None, HeadState::Branch { .. }) => {
            return Err(mismatch());
        }
        (_, HeadState::Invalid) => return Err(mismatch()),
    }
    Ok(())
}

/// The on-disk index before an apply, kept for rollback and crash recovery.
///
/// The file's mtime is part of the snapshot: entries stamped in the same second as the index are
/// only known to need a content check because of it, so a restore must put it back.
pub(crate) struct IndexSnapshot {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mtime: Option<std::time::SystemTime>,
}

impl IndexSnapshot {
    /// Write the snapshot over the index at `path`, restoring its mtime.
    fn restore_to(&self, path: &Path) -> std::io::Result<()> {
        fs::write(path, &self.bytes)?;
        if let Some(mtime) = self.mtime {
            fs::File::options()
                .write(true)
                .open(path)?
                .set_modified(mtime)?;
        }
        Ok(())
    }
}

fn capture_index_snapshot(repo: &Repository) -> Result<IndexSnapshot, HistoryError> {
    let io_err = |e: std::io::Error| HistoryError::WorktreeApply {
        message: e.to_string(),
    };
    let path = repo.index_path();
    if path.is_file() {
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        let bytes = fs::read(path).map_err(io_err)?;
        return Ok(IndexSnapshot { bytes, mtime });
    }
    let mut index = repo.load_index().map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    let tmp = tempfile::NamedTempFile::new().map_err(io_err)?;
    index
        .write_to_path(tmp.path(), false)
        .map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
    let bytes = fs::read(tmp.path()).map_err(io_err)?;
    Ok(IndexSnapshot { bytes, mtime: None })
}

fn head_tree_transition(plan: &RewritePlan) -> (Option<ObjectId>, Option<ObjectId>) {
    (plan.head.old_tree, plan.head.new_tree)
}

fn head_tree_changes(old_tree: &Option<ObjectId>, new_tree: &Option<ObjectId>) -> bool {
    match (old_tree, new_tree) {
        (Some(old), Some(new)) => old != new,
        _ => false,
    }
}

fn verify_worktree_ready_for_head_tree_change(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<(), HistoryError> {
    prepare_tree_checkout(repo, from_tree, to_tree).map_err(map_prepare_checkout_error)?;
    match verify_tree_transition_allow_local(repo, from_tree, to_tree) {
        Ok(()) => Ok(()),
        Err(TreeTransitionVerifyError::WouldOverwrite(paths)) => {
            Err(HistoryError::LocalChangesWouldBeOverwritten { paths })
        }
        Err(TreeTransitionVerifyError::Failed(err)) => Err(map_prepare_checkout_error(err)),
    }
}

fn map_prepare_checkout_error(err: crate::error::Error) -> HistoryError {
    if let crate::error::Error::WouldOverwrite { paths, .. } = err {
        return HistoryError::LocalChangesWouldBeOverwritten { paths };
    }
    HistoryError::WorktreeApply {
        message: err.to_string(),
    }
}

fn apply_head_tree_to_worktree(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
    index_lock: Option<&mut crate::index::IndexLock>,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<(), HistoryError> {
    checkout_tree_transition_allow_local_with_preserved(
        repo,
        from_tree,
        to_tree,
        &std::collections::HashSet::new(),
        index_lock,
        progress,
        cancel,
    )
    .map_err(|e| match e {
        crate::error::Error::Cancelled => HistoryError::from(e),
        other => HistoryError::WorktreeApply {
            message: other.to_string(),
        },
    })
}

fn commit_planned_ref_updates(
    repo: &Repository,
    updates: &[PlannedRefUpdate],
    opts: &ApplyOptions,
) -> Result<(), HistoryError> {
    let store = repo.refs();
    let git_dir = repo.git_dir();
    let config = repo.config().ok();
    let head_target =
        read_symbolic_ref_for_repository(repo, "HEAD").map_err(|e| HistoryError::RefApply {
            message: e.to_string(),
        })?;
    let mut txn = RefTransaction::new();
    for planned in updates {
        let reflog = reflog_for_update(
            config.as_ref().map(|c| c.as_ref()),
            git_dir,
            &planned.name,
            opts,
        );
        if head_target.as_deref() == Some(planned.name.as_str()) {
            if let Some(head_update) =
                head_log_update(config.as_ref().map(|c| c.as_ref()), git_dir, planned, opts)
            {
                txn = txn
                    .update(head_update)
                    .map_err(|e| map_ref_store_error(e, "HEAD"))?;
            }
        }
        let update = RefUpdate {
            name: planned.name.clone(),
            new_value: Some(RawRef::Direct(planned.new)),
            expected: Expected::Oid(planned.old),
            reflog,
            flags: ref_update_flags(&planned.name),
        };
        txn = txn
            .update(update)
            .map_err(|e| map_ref_store_error(e, &planned.name))?;
    }
    let wait = LockWait::files_ref_lock(config.as_ref().map(|c| c.as_ref()));
    let prepared = prepare_with_lock_wait(store, txn, wait).map_err(|e| {
        map_ref_store_error(
            e,
            updates.first().map(|u| u.name.as_str()).unwrap_or("refs"),
        )
    })?;
    prepared.commit().map_err(|e| {
        map_ref_store_error(
            e,
            updates.first().map(|u| u.name.as_str()).unwrap_or("refs"),
        )
    })?;
    Ok(())
}

fn rollback_ref_updates(
    repo: &Repository,
    updates: &[PlannedRefUpdate],
    opts: &ApplyOptions,
) -> Result<(), HistoryError> {
    if updates.is_empty() {
        return Ok(());
    }
    let inverse: Vec<PlannedRefUpdate> = updates
        .iter()
        .map(|u| PlannedRefUpdate {
            name: u.name.clone(),
            old: u.new,
            new: u.old,
        })
        .collect();
    commit_planned_ref_updates(repo, &inverse, opts)
}

/// Append one `HEAD` reflog entry for `branch` moving `old` → `new`, as if `HEAD` had been
/// attached to that branch when it moved.
///
/// Used by callers that move the checked-out branch while `HEAD` is detached (a resumed rebase)
/// and reattach it afterwards, so the `HEAD` log still records the move. Logging follows the
/// same policy as [`apply_rewrite`]; nothing is written when that policy skips `HEAD`.
pub(super) fn log_head_move(
    repo: &Repository,
    branch: &PlannedRefUpdate,
    opts: &ApplyOptions,
) -> Result<(), HistoryError> {
    let config = repo.config().ok();
    let Some(update) = head_log_update(
        config.as_ref().map(|c| c.as_ref()),
        repo.git_dir(),
        branch,
        opts,
    ) else {
        return Ok(());
    };
    let txn = RefTransaction::new()
        .update(update)
        .map_err(|e| map_ref_store_error(e, "HEAD"))?;
    let wait = LockWait::files_ref_lock(config.as_ref().map(|c| c.as_ref()));
    prepare_with_lock_wait(repo.refs(), txn, wait)
        .and_then(|prepared| prepared.commit())
        .map_err(|e| map_ref_store_error(e, "HEAD"))?;
    Ok(())
}

/// Log-only `HEAD` update mirroring `branch`, or `None` when `HEAD` is not logged.
fn head_log_update(
    config: Option<&ConfigSet>,
    git_dir: &std::path::Path,
    branch: &PlannedRefUpdate,
    opts: &ApplyOptions,
) -> Option<RefUpdate> {
    let mut reflog = reflog_for_update(config, git_dir, "HEAD", opts)?;
    reflog.old_oid = Some(branch.old);
    reflog.new_oid = Some(branch.new);
    Some(RefUpdate {
        name: "HEAD".into(),
        new_value: Some(RawRef::Direct(branch.new)),
        expected: Expected::Any,
        reflog: Some(reflog),
        flags: RefUpdateFlags {
            log_only: true,
            no_deref: true,
            ..Default::default()
        },
    })
}

fn reflog_for_update(
    config: Option<&ConfigSet>,
    git_dir: &std::path::Path,
    refname: &str,
    opts: &ApplyOptions,
) -> Option<ReflogUpdate> {
    let force = opts.record_operation.is_some();
    let autocreate =
        config.is_some_and(|cfg| should_autocreate_reflog_with_config(cfg, git_dir, refname));
    if force || autocreate {
        Some(ReflogUpdate {
            identity: opts.identity.reflog_name_email(),
            message: opts.reflog_message.clone(),
            time: opts.identity.time,
            old_oid: None,
            new_oid: None,
        })
    } else {
        None
    }
}

fn map_ref_store_error(err: RefStoreError, label: &str) -> HistoryError {
    match err {
        RefStoreError::ExpectedMismatch { name, .. } => HistoryError::RefCasMismatch { name },
        other => HistoryError::RefApply {
            message: format!("{label}: {other}"),
        },
    }
}

/// Commit `HEAD` resolves to once `updates` are committed, read through the repository's ref
/// store before any ref moves so a failure leaves every ref untouched.
fn head_commit_after(
    repo: &Repository,
    updates: &[PlannedRefUpdate],
) -> Result<ObjectId, HistoryError> {
    let head = resolve_head_for_repository(repo)?;
    let head_name = match &head {
        HeadState::Branch { refname, .. } => refname.as_str(),
        HeadState::Detached { .. } | HeadState::Invalid => "HEAD",
    };
    if let Some(update) = updates.iter().find(|u| u.name == head_name) {
        return Ok(update.new);
    }
    match head {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => Ok(oid),
        HeadState::Branch { oid: None, .. } => Err(crate::error::Error::UnbornHead.into()),
        HeadState::Invalid => Err(crate::error::Error::InvalidHead.into()),
    }
}

fn apply_failure_after_ref_update(
    checkout: HistoryError,
    ref_rollback: Option<HistoryError>,
    worktree_restore: Option<HistoryError>,
) -> HistoryError {
    if checkout.is_cancelled() && ref_rollback.is_none() && worktree_restore.is_none() {
        return checkout;
    }
    let checkout_message = match checkout {
        HistoryError::WorktreeApply { message } => message,
        other => other.to_string(),
    };
    if ref_rollback.is_none() && worktree_restore.is_none() {
        return HistoryError::WorktreeApply {
            message: checkout_message,
        };
    }
    HistoryError::ApplyRollbackIncomplete {
        checkout: checkout_message,
        ref_rollback: ref_rollback.map(|e| e.to_string()),
        worktree_restore: worktree_restore.map(|e| e.to_string()),
    }
}

/// Worktree presence captured before a tree transition checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreePathKind {
    /// Path did not exist in the worktree.
    Absent,
    /// Path was a directory (possibly implicit via tracked children).
    Directory,
    /// Path was a regular file (`bytes` holds contents).
    Regular,
    /// Path was a symlink (`bytes` holds the link target).
    Symlink,
}

/// Pre-checkout state for one path in the forward tree transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreePathSnapshot {
    pub(crate) kind: WorktreePathKind,
    pub(crate) bytes: Option<Vec<u8>>,
    /// When set, rollback rebuilds this path from the journal's pre-apply `head-tree` OID.
    pub(crate) from_old_tree: bool,
    #[cfg(unix)]
    mode: u32,
}

impl WorktreePathSnapshot {
    /// No file at the path.
    pub(crate) fn absent() -> Self {
        Self::from_parts(WorktreePathKind::Absent, None, 0)
    }

    /// The file a checkout of a blob with Git `git_mode` writes (`0` permissions mean the
    /// process default).
    pub(crate) fn file(git_mode: u32, bytes: Vec<u8>) -> Self {
        match git_mode {
            0o120000 => Self::from_parts(WorktreePathKind::Symlink, Some(bytes), 0),
            0o100755 => Self::from_parts(WorktreePathKind::Regular, Some(bytes), 0o755),
            _ => Self::from_parts(WorktreePathKind::Regular, Some(bytes), 0),
        }
    }

    /// Reassemble a snapshot from its stored parts; `unix_mode` is ignored off Unix.
    pub(crate) fn from_parts(
        kind: WorktreePathKind,
        bytes: Option<Vec<u8>>,
        unix_mode: u32,
    ) -> Self {
        #[cfg(not(unix))]
        let _ = unix_mode;
        Self {
            kind,
            bytes,
            from_old_tree: false,
            #[cfg(unix)]
            mode: unix_mode,
        }
    }

    /// Pre-apply path that matches the old tree; rollback materializes it from the ODB.
    pub(crate) fn from_old_tree(kind: WorktreePathKind) -> Self {
        #[cfg(unix)]
        let mode = match kind {
            WorktreePathKind::Directory => 0o755,
            WorktreePathKind::Regular => 0o644,
            _ => 0,
        };
        Self {
            kind,
            bytes: None,
            from_old_tree: true,
            #[cfg(unix)]
            mode,
        }
    }

    /// Permission bits recorded for the path (`0` when unknown or off Unix).
    pub(crate) fn unix_mode(&self) -> u32 {
        #[cfg(unix)]
        {
            self.mode
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    pub(crate) fn is_executable(&self) -> bool {
        self.kind == WorktreePathKind::Regular && self.unix_mode() & 0o111 != 0
    }

    /// Whether this file is what checking out a blob of `git_mode` with `data` produces.
    pub(crate) fn content_matches(&self, git_mode: u32, data: &[u8]) -> bool {
        let Some(bytes) = self.bytes.as_deref() else {
            return false;
        };
        let mode_matches = match self.kind {
            WorktreePathKind::Symlink => git_mode == 0o120000,
            WorktreePathKind::Regular if self.is_executable() => git_mode == 0o100755,
            WorktreePathKind::Regular => git_mode == 0o100644,
            WorktreePathKind::Absent | WorktreePathKind::Directory => false,
        };
        mode_matches && bytes == data
    }
}

fn snapshot_worktree_for_tree_transition(
    repo: &Repository,
    from_tree: &ObjectId,
    to_tree: &ObjectId,
) -> Result<HashMap<String, WorktreePathSnapshot>, HistoryError> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| HistoryError::WorktreeApply {
            message: "cannot snapshot worktree in a bare repository".into(),
        })?;
    let prepared =
        prepare_tree_transition_allow_local(repo, Some(from_tree), to_tree, &HashSet::new())
            .map_err(|err| match err {
                TreeTransitionPrepareError::Failed(e) => HistoryError::WorktreeApply {
                    message: e.to_string(),
                },
                TreeTransitionPrepareError::OverwritePaths(paths) => {
                    HistoryError::LocalChangesWouldBeOverwritten { paths }
                }
            })?;
    let preserve = prepared.preserve_worktree_paths();
    let index = prepared.index();
    let mut out = HashMap::new();
    for change in prepared.tree_changes() {
        let Some(path) = tree_transition_path(change) else {
            continue;
        };
        if index
            .get(path.as_bytes(), 0)
            .is_some_and(|entry| entry.mode == crate::index::MODE_GITLINK)
        {
            continue;
        }
        let snap = if preserve.contains(path) {
            read_worktree_path_snapshot(work_tree, path).map_err(|e| {
                HistoryError::WorktreeApply {
                    message: format!("snapshot worktree path '{path}': {e}"),
                }
            })?
        } else {
            worktree_snapshot_from_tree_change(change)?
        };
        out.insert(path.to_owned(), snap);
    }
    Ok(out)
}

fn worktree_snapshot_from_tree_change(
    change: &DiffEntry,
) -> Result<WorktreePathSnapshot, HistoryError> {
    if change.status == DiffStatus::Added {
        return Ok(WorktreePathSnapshot::from_old_tree(
            WorktreePathKind::Absent,
        ));
    }
    let mode = change.old_mode.bits();
    let kind = match mode {
        crate::index::MODE_TREE | crate::index::MODE_GITLINK => WorktreePathKind::Directory,
        0o120000 => WorktreePathKind::Symlink,
        _ => WorktreePathKind::Regular,
    };
    Ok(WorktreePathSnapshot::from_old_tree(kind))
}

fn tree_transition_path(change: &crate::diff::DiffEntry) -> Option<&str> {
    change.new_path.as_deref().or(change.old_path.as_deref())
}

pub(crate) fn read_worktree_path_snapshot(
    work_tree: &Path,
    path: &str,
) -> Result<WorktreePathSnapshot, std::io::Error> {
    let abs = work_tree.join(path);
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorktreePathSnapshot {
                kind: WorktreePathKind::Absent,
                bytes: None,
                from_old_tree: false,
                #[cfg(unix)]
                mode: 0o644,
            });
        }
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(&abs)?;
        return Ok(WorktreePathSnapshot {
            kind: WorktreePathKind::Symlink,
            bytes: Some(target.as_os_str().as_encoded_bytes().to_vec()),
            from_old_tree: false,
            #[cfg(unix)]
            mode: 0,
        });
    }
    if meta.is_file() {
        let bytes = fs::read(&abs)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o7777;
            return Ok(WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(bytes),
                from_old_tree: false,
                mode,
            });
        }
        #[cfg(not(unix))]
        {
            return Ok(WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(bytes),
                from_old_tree: false,
            });
        }
    }
    Ok(WorktreePathSnapshot {
        kind: WorktreePathKind::Directory,
        bytes: None,
        from_old_tree: false,
        #[cfg(unix)]
        mode: 0o755,
    })
}

pub(crate) fn restore_worktree_after_failed_apply(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    index_snapshot: Option<&IndexSnapshot>,
    worktree_snapshot: Option<&HashMap<String, WorktreePathSnapshot>>,
) -> Result<(), HistoryError> {
    if let Some(snapshot) = index_snapshot {
        snapshot
            .restore_to(&repo.index_path())
            .map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
    }
    let Some(work_tree) = repo.work_tree() else {
        return Ok(());
    };
    let Some(snapshot) = worktree_snapshot else {
        return Ok(());
    };
    let mut paths: Vec<&String> = snapshot.keys().collect();
    paths.sort_by(|a, b| path_depth_cmp(a, b).reverse());
    for path in &paths {
        let saved = &snapshot[*path];
        let abs = work_tree.join(path.as_str());
        clear_worktree_path_before_restore(&abs, saved).map_err(|e| {
            HistoryError::WorktreeApply {
                message: format!("failed to clear '{path}' before restore: {e}"),
            }
        })?;
    }
    paths.sort_by(|a, b| path_depth_cmp(a, b));
    for path in &paths {
        let saved = &snapshot[*path];
        if saved.from_old_tree {
            let old_tree = old_tree.ok_or_else(|| HistoryError::WorktreeApply {
                message: "missing head-tree for from-tree worktree restore".into(),
            })?;
            restore_worktree_path_from_old_tree(repo, work_tree, path.as_str(), old_tree, saved)
                .map_err(|e| HistoryError::WorktreeApply {
                    message: format!("failed to restore '{path}' from tree: {e}"),
                })?;
        } else {
            let abs = work_tree.join(path.as_str());
            restore_worktree_path(&abs, saved).map_err(|e| HistoryError::WorktreeApply {
                message: format!("failed to restore '{path}': {e}"),
            })?;
        }
    }
    Ok(())
}

fn restore_worktree_path_from_old_tree(
    repo: &Repository,
    work_tree: &Path,
    path: &str,
    old_tree: &ObjectId,
    saved: &WorktreePathSnapshot,
) -> Result<(), crate::error::Error> {
    let abs = work_tree.join(path);
    clear_worktree_path_before_restore(&abs, saved)?;
    match saved.kind {
        WorktreePathKind::Absent => Ok(()),
        WorktreePathKind::Directory => {
            fs::create_dir_all(&abs)?;
            Ok(())
        }
        WorktreePathKind::Regular | WorktreePathKind::Symlink => {
            let Some((oid, mode)) = tree_entry_at_path(repo, *old_tree, path)? else {
                return Ok(());
            };
            if mode == crate::index::MODE_GITLINK {
                return materialize_gitlink_worktree(work_tree, path, &mut LeadingDirCache::new());
            }
            let index = repo.load_index()?;
            let config = repo.config()?;
            let rules = WorktreeRules::from_parts_index_attributes_only(repo, &index, config)?;
            write_blob_to_worktree(
                repo,
                &rules,
                work_tree,
                path,
                &oid,
                mode,
                &mut LeadingDirCache::new(),
            )?;
            Ok(())
        }
    }
}

fn path_depth_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let depth = |p: &str| p.split('/').filter(|s| !s.is_empty()).count();
    depth(a).cmp(&depth(b)).then_with(|| a.cmp(b))
}

fn clear_worktree_path_before_restore(
    abs: &Path,
    saved: &WorktreePathSnapshot,
) -> std::io::Result<()> {
    match saved.kind {
        WorktreePathKind::Absent => {
            if abs.is_symlink() || abs.is_file() {
                fs::remove_file(abs)?;
            } else if abs.is_dir() {
                fs::remove_dir_all(abs)?;
            }
        }
        WorktreePathKind::Directory => {
            if abs.is_symlink() || abs.is_file() {
                fs::remove_file(abs)?;
            }
        }
        WorktreePathKind::Regular | WorktreePathKind::Symlink => {
            if abs.is_dir() {
                fs::remove_dir_all(abs)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn restore_worktree_path(
    abs: &Path,
    saved: &WorktreePathSnapshot,
) -> std::io::Result<()> {
    if !matches!(saved.kind, WorktreePathKind::Absent) {
        clear_worktree_path_before_restore(abs, saved)?;
    }
    match saved.kind {
        WorktreePathKind::Absent => Ok(()),
        WorktreePathKind::Directory => {
            fs::create_dir_all(abs)?;
            Ok(())
        }
        WorktreePathKind::Symlink => {
            let Some(bytes) = saved.bytes.as_ref() else {
                return Ok(());
            };
            if abs.is_symlink() || abs.is_file() {
                fs::remove_file(abs)?;
            }
            #[cfg(unix)]
            {
                use std::ffi::OsStr;
                use std::os::unix::ffi::OsStrExt;
                use std::os::unix::fs::symlink;
                let target = OsStr::from_bytes(bytes);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                symlink(target, abs)?;
            }
            #[cfg(not(unix))]
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "symlink restore requires unix",
                ));
            }
            Ok(())
        }
        WorktreePathKind::Regular => {
            let Some(bytes) = saved.bytes.as_ref() else {
                return Ok(());
            };
            if abs.is_dir() {
                fs::remove_dir_all(abs)?;
            } else if abs.is_symlink() || abs.is_file() {
                fs::remove_file(abs)?;
            }
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(abs, bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if saved.mode != 0 {
                    let mut perms = fs::metadata(abs)?.permissions();
                    perms.set_mode(saved.mode);
                    fs::set_permissions(abs, perms)?;
                }
            }
            Ok(())
        }
    }
}

#[path = "rewrite_journal.rs"]
mod rewrite_journal;

pub use rewrite_journal::{
    abort_interrupted_rewrite_apply, ensure_rewrite_apply_ready, recover_interrupted_rewrite_apply,
    rewrite_apply_journal_active, RewriteApplyRecovery,
};

#[cfg(test)]
mod inject_and_apply_tests {
    use std::fs;
    use std::ops::ControlFlow;
    use std::sync::Arc;

    use crate::history::{
        plan_rewrite, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
    };
    use crate::objects::{parse_commit, ObjectKind};
    use crate::reflog::ReflogEntry;
    use crate::refs::store::{
        MemoryRefStore, PreparedRefTransaction, RawRef, RefStore, RefStoreError, RefTransaction,
    };
    use crate::repo::init_repository;
    use crate::RefStorageFormat;

    use super::*;

    #[derive(Debug)]
    struct FailOnPrepareStore {
        inner: MemoryRefStore,
    }

    impl RefStore for FailOnPrepareStore {
        fn format(&self) -> crate::refs::store::RefStorageFormat {
            self.inner.format()
        }

        fn read_raw(&self, name: &str) -> crate::error::Result<Option<RawRef>> {
            self.inner.read_raw(name)
        }

        fn for_each_ref(
            &self,
            prefix: &str,
            f: &mut dyn FnMut(&crate::refs::store::RefEntry) -> ControlFlow<()>,
        ) -> crate::error::Result<()> {
            self.inner.for_each_ref(prefix, f)
        }

        fn prepare(
            &self,
            _txn: RefTransaction,
        ) -> Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
            Err(RefStoreError::Corrupt("injected ref apply failure".into()))
        }

        fn reflog_exists(&self, name: &str) -> crate::error::Result<bool> {
            self.inner.reflog_exists(name)
        }

        fn for_each_reflog_entry(
            &self,
            name: &str,
            reverse: bool,
            f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
        ) -> crate::error::Result<()> {
            self.inner.for_each_reflog_entry(name, reverse, f)
        }

        fn create_reflog(&self, name: &str) -> crate::error::Result<()> {
            self.inner.create_reflog(name)
        }

        fn delete_reflog(&self, name: &str) -> crate::error::Result<()> {
            self.inner.delete_reflog(name)
        }

        fn replace_reflog(
            &self,
            name: &str,
            entries: Vec<ReflogEntry>,
        ) -> crate::error::Result<()> {
            self.inner.replace_reflog(name, entries)
        }

        fn rewrite_reflog(
            &self,
            name: &str,
            transform: &mut dyn FnMut(Vec<ReflogEntry>) -> crate::error::Result<Vec<ReflogEntry>>,
        ) -> crate::error::Result<()> {
            self.inner.rewrite_reflog(name, transform)
        }

        fn for_each_reflog_ref(
            &self,
            f: &mut dyn FnMut(&str) -> ControlFlow<()>,
        ) -> crate::error::Result<()> {
            self.inner.for_each_reflog_ref(f)
        }
    }

    #[test]
    fn apply_rewrite_injected_ref_failure_restores_worktree_and_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        repo.odb().write(ObjectKind::Tree, &[]).expect("tree");
        let blob = repo.odb().write(ObjectKind::Blob, b"file\n").expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "file\n").expect("write");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let child_tree = crate::index::write_tree::write_tree_from_index(
            repo.odb(),
            &{
                let mut idx = index.clone();
                let blob2 = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
                idx.add_or_replace(crate::index::IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: crate::index::MODE_REGULAR,
                    uid: 0,
                    gid: 0,
                    size: 4,
                    oid: blob2,
                    flags: 8,
                    flags_extended: None,
                    path: b"two.txt".to_vec(),
                    base_index_pos: 0,
                });
                idx
            },
            "",
        )
        .expect("child tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        let store = Arc::new(FailOnPrepareStore {
            inner: MemoryRefStore::new(),
        });
        for update in &plan.ref_updates {
            store
                .inner
                .set_ref(&update.name, RawRef::Direct(update.old))
                .expect("seed");
        }
        store
            .inner
            .set_ref("HEAD", RawRef::Symbolic("refs/heads/main".to_owned()))
            .expect("head");
        let repo = repo.with_ref_store(store);
        let before_index = fs::read(repo.index_path()).expect("index");
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("inject");
        assert!(matches!(err, HistoryError::RefApply { .. }));
        assert_eq!(fs::read(repo.index_path()).expect("index"), before_index);
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );
        let head_tree = parse_commit(&repo.odb().read(&root).expect("read").data)
            .expect("parse")
            .tree;
        assert_eq!(
            parse_commit(
                &repo
                    .odb()
                    .read(&crate::refs::resolve_ref(repo.git_dir(), "HEAD").expect("head"))
                    .expect("read")
                    .data
            )
            .expect("parse")
            .tree,
            head_tree
        );
    }

    #[test]
    fn apply_rewrite_injected_checkout_failure_restores_worktree_and_index() {
        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let blob = repo.odb().write(ObjectKind::Blob, b"file\n").expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "file\n").expect("write");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let blob2 = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
        let mut idx2 = index.clone();
        idx2.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 4,
            oid: blob2,
            flags: 8,
            flags_extended: None,
            path: b"two.txt".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &idx2, "").expect("tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        let before_index = fs::read(repo.index_path()).expect("index");
        set_test_inject_index_write_fail_remaining(1);
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("inject checkout failure");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(
            err,
            HistoryError::WorktreeApply { .. } | HistoryError::ApplyRollbackIncomplete { .. }
        ));
        let tree_from_index_path = |path: &std::path::Path| {
            let index = crate::index::Index::load(path).expect("index");
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree")
        };
        let before_index_path = dir.path().join(".git/index.before");
        fs::write(&before_index_path, &before_index).expect("snapshot");
        assert_eq!(
            tree_from_index_path(repo.index_path().as_path()),
            tree_from_index_path(&before_index_path)
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );
        assert!(dir.path().join("file.txt").exists());
        assert!(
            !dir.path().join("two.txt").exists(),
            "partial checkout must be rolled back"
        );
    }

    #[test]
    fn apply_index_to_worktree_delta_preserves_middle_insertion() {
        let old_index = b"one\nstaged B\nthree\n";
        let old_worktree = b"one\nstaged B\nunstaged C\nthree\n";
        let new_index = b"one\nstaged B\nthree\n";
        let got = apply_index_to_worktree_delta(old_index, old_worktree, new_index);
        assert_eq!(got, old_worktree);
    }

    #[test]
    fn apply_index_to_worktree_delta_preserves_trailing_suffix() {
        let old_index = b"base\nstaged B\n";
        let old_worktree = b"base\nstaged B\nunstaged C\n";
        let new_index = b"base\nstaged B\n";
        let got = apply_index_to_worktree_delta(old_index, old_worktree, new_index);
        assert_eq!(got, old_worktree);
    }

    #[test]
    fn apply_index_to_worktree_delta_replays_onto_changed_staged_content() {
        let old_index = b"one\nstaged B\nthree\n";
        let old_worktree = b"one\nstaged B\nunstaged C\nthree\n";
        let new_index = b"one\nstaged B2\nthree\n";
        let got = apply_index_to_worktree_delta(old_index, old_worktree, new_index);
        assert_eq!(got, b"one\nstaged B2\nunstaged C\nthree\n");
    }

    #[test]
    fn apply_fixup_rewrite_injected_ref_failure_restores_worktree_and_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let blob = repo.odb().write(ObjectKind::Blob, b"base\n").expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "base\n").expect("write");
        let root_commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &root_commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let blob_staged = repo
            .odb()
            .write(ObjectKind::Blob, b"base\nstaged\n")
            .expect("blob");
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 11,
            oid: blob_staged,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "base\nstaged\n").expect("write");

        let child_tree = crate::index::write_tree::write_tree_from_index(
            repo.odb(),
            &{
                let mut idx = index.clone();
                let blob2 = repo.odb().write(ObjectKind::Blob, b"tip\n").expect("blob");
                idx.add_or_replace(crate::index::IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: crate::index::MODE_REGULAR,
                    uid: 0,
                    gid: 0,
                    size: 4,
                    oid: blob2,
                    flags: 7,
                    flags_extended: None,
                    path: b"tip.txt".to_vec(),
                    base_index_pos: 0,
                });
                idx
            },
            "",
        )
        .expect("child tree");
        let tip_commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree: child_tree,
            parents: vec![root],
            author: "T <t@t> 1 +0000".into(),
            committer: "T <t@t> 1 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "tip\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let tip = repo
            .odb()
            .write(ObjectKind::Commit, &tip_commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");

        let merged_tree = crate::index::write_tree::write_tree_from_index(
            repo.odb(),
            &{
                let mut idx = index.clone();
                let merged_blob = repo
                    .odb()
                    .write(ObjectKind::Blob, b"base\nstaged\n")
                    .expect("blob");
                idx.add_or_replace(crate::index::IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: crate::index::MODE_REGULAR,
                    uid: 0,
                    gid: 0,
                    size: 11,
                    oid: merged_blob,
                    flags: 8,
                    flags_extended: None,
                    path: b"file.txt".to_vec(),
                    base_index_pos: 0,
                });
                let tip_blob = repo.odb().write(ObjectKind::Blob, b"tip\n").expect("blob");
                idx.add_or_replace(crate::index::IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: crate::index::MODE_REGULAR,
                    uid: 0,
                    gid: 0,
                    size: 4,
                    oid: tip_blob,
                    flags: 7,
                    flags_extended: None,
                    path: b"tip.txt".to_vec(),
                    base_index_pos: 0,
                });
                idx
            },
            "",
        )
        .expect("merged");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(merged_tree),
                message: None,
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 2 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        let store = Arc::new(FailOnPrepareStore {
            inner: MemoryRefStore::new(),
        });
        for update in &plan.ref_updates {
            store
                .inner
                .set_ref(&update.name, RawRef::Direct(update.old))
                .expect("seed");
        }
        store
            .inner
            .set_ref("HEAD", RawRef::Symbolic("refs/heads/main".to_owned()))
            .expect("head");
        let repo = repo.with_ref_store(store);
        let before_index = fs::read(repo.index_path()).expect("index");
        let before_worktree = fs::read(dir.path().join("file.txt")).expect("wt");
        let staged_paths = vec!["file.txt".to_owned()];
        let err = apply_fixup_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "fixup".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &staged_paths,
        )
        .expect_err("inject");
        assert!(matches!(err, HistoryError::RefApply { .. }));
        assert_eq!(fs::read(repo.index_path()).expect("index"), before_index);
        assert_eq!(
            fs::read(dir.path().join("file.txt")).expect("wt"),
            before_worktree
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            tip
        );
    }

    #[test]
    fn restore_worktree_path_replaces_directory_with_saved_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tracked = dir.path().join("tracked");
        fs::create_dir_all(&tracked).expect("mkdir");
        fs::write(tracked.join("child"), "nested\n").expect("child");
        let saved = WorktreePathSnapshot {
            kind: WorktreePathKind::Regular,
            bytes: Some(b"original file\n".to_vec()),
            from_old_tree: false,
            #[cfg(unix)]
            mode: 0o100644,
        };
        restore_worktree_path(&tracked, &saved).expect("restore regular file");
        assert!(tracked.is_file());
        assert!(!tracked.is_dir());
        assert_eq!(
            fs::read_to_string(&tracked).expect("read"),
            "original file\n"
        );
        assert!(
            !tracked.join("child").exists(),
            "directory subtree from partial checkout must be removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_worktree_path_replaces_symlink_with_saved_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link_target = dir.path().join("link-target.txt");
        fs::write(&link_target, "target content\n").expect("target");
        let tracked = dir.path().join("tracked.txt");
        std::os::unix::fs::symlink("link-target.txt", &tracked).expect("symlink");
        let saved = WorktreePathSnapshot {
            kind: WorktreePathKind::Regular,
            bytes: Some(b"original file\n".to_vec()),
            from_old_tree: false,
            mode: 0o100644,
        };
        restore_worktree_path(&tracked, &saved).expect("restore");
        assert!(tracked.is_file());
        assert!(!tracked.is_symlink());
        assert_eq!(
            fs::read_to_string(tracked).expect("read"),
            "original file\n"
        );
        assert_eq!(
            fs::read_to_string(link_target).expect("read target"),
            "target content\n",
            "must not follow symlink and overwrite the external target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_worktree_path_snapshot_propagates_unreadable_regular_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.txt");
        fs::write(&path, "secret\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
        let err = read_worktree_path_snapshot(dir.path(), "secret.txt").expect_err("read denied");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("restore mode");
    }

    #[cfg(unix)]
    #[test]
    fn restore_worktree_symlink_preserves_non_utf8_target_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("link");
        let target_bytes: &[u8] = b"target-\xff-bytes";
        std::os::unix::fs::symlink(OsStr::from_bytes(target_bytes), &link).expect("symlink");
        let saved = read_worktree_path_snapshot(dir.path(), "link").expect("snapshot");
        assert_eq!(saved.kind, WorktreePathKind::Symlink);
        assert_eq!(saved.bytes.as_deref(), Some(target_bytes));

        fs::remove_file(&link).expect("remove");
        restore_worktree_path(&link, &saved).expect("restore");
        let restored = fs::read_link(&link).expect("read link");
        assert_eq!(restored.as_os_str().as_encoded_bytes(), target_bytes);
    }

    #[test]
    fn restore_worktree_after_failed_apply_directory_to_file_partial_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        fs::write(dir.path().join("tracked"), "partial file\n").expect("partial file");

        let mut snapshot = HashMap::new();
        snapshot.insert(
            "tracked".into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Directory,
                bytes: None,
                from_old_tree: false,
                #[cfg(unix)]
                mode: 0o755,
            },
        );
        snapshot.insert(
            "tracked/child".into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(b"child\n".to_vec()),
                from_old_tree: false,
                #[cfg(unix)]
                mode: 0o644,
            },
        );

        restore_worktree_after_failed_apply(&repo, None, None, Some(&snapshot)).expect("restore");

        let tracked = dir.path().join("tracked");
        let child = dir.path().join("tracked/child");
        assert!(tracked.is_dir());
        assert!(child.is_file());
        assert_eq!(fs::read_to_string(child).expect("read child"), "child\n");
        assert!(!tracked.is_file());
    }

    #[test]
    fn apply_rewrite_injected_checkout_failure_restores_directory_after_file_transition() {
        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let child_blob = repo
            .odb()
            .write(ObjectKind::Blob, b"child\n")
            .expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 6,
            oid: child_blob,
            flags: 14,
            flags_extended: None,
            path: b"tracked/child".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::create_dir_all(dir.path().join("tracked")).expect("mkdir");
        fs::write(dir.path().join("tracked/child"), "child\n").expect("write child");

        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let tracked_blob = repo
            .odb()
            .write(ObjectKind::Blob, b"replacement\n")
            .expect("tracked blob");
        let mut idx2 = crate::index::Index::new();
        idx2.hash_algo = repo.odb().hash_algo();
        idx2.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 12,
            oid: tracked_blob,
            flags: 8,
            flags_extended: None,
            path: b"tracked".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &idx2, "").expect("tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        let before_index = fs::read(repo.index_path()).expect("index");
        set_test_inject_index_write_fail_remaining(1);
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("inject checkout failure");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(err, HistoryError::WorktreeApply { .. }));

        let tree_from_index_path = |path: &std::path::Path| {
            let index = crate::index::Index::load(path).expect("index");
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree")
        };
        let before_index_path = dir.path().join(".git/index.before");
        fs::write(&before_index_path, &before_index).expect("snapshot");
        assert_eq!(
            tree_from_index_path(repo.index_path().as_path()),
            tree_from_index_path(&before_index_path)
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );

        let tracked = dir.path().join("tracked");
        let child = dir.path().join("tracked/child");
        assert!(tracked.is_dir(), "rollback must restore directory tracked/");
        assert!(!tracked.is_file());
        assert!(child.is_file());
        assert_eq!(fs::read_to_string(child).expect("read child"), "child\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_rewrite_injected_checkout_failure_restores_regular_file_after_symlink_transition() {
        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let blob = repo.odb().write(ObjectKind::Blob, b"file\n").expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "file\n").expect("write");
        let link_target_path = dir.path().join("link-target.txt");
        fs::write(&link_target_path, "target content\n").expect("link target");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let link_blob = repo
            .odb()
            .write(ObjectKind::Blob, b"link-target.txt")
            .expect("link blob");
        let mut idx2 = index.clone();
        idx2.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_SYMLINK,
            uid: 0,
            gid: 0,
            size: 14,
            oid: link_blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &idx2, "").expect("tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        set_test_inject_index_write_fail_remaining(1);
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("inject checkout failure");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(err, HistoryError::WorktreeApply { .. }));

        let file_path = dir.path().join("file.txt");
        assert!(file_path.is_file());
        assert!(!file_path.is_symlink());
        assert_eq!(fs::read_to_string(&file_path).expect("read file"), "file\n");
        assert_eq!(
            fs::read_to_string(&link_target_path).expect("read target"),
            "target content\n"
        );
    }

    #[test]
    fn apply_rewrite_injected_checkout_failure_restores_regular_file_after_directory_transition() {
        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let blob = repo.odb().write(ObjectKind::Blob, b"file\n").expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "file\n").expect("write");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let nested = repo
            .odb()
            .write(ObjectKind::Blob, b"nested\n")
            .expect("nested blob");
        let mut idx2 = index.clone();
        idx2.remove_paths([b"file.txt".as_slice()]);
        idx2.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 7,
            oid: nested,
            flags: 8,
            flags_extended: None,
            path: b"file.txt/child".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &idx2, "").expect("tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        let before_index = fs::read(repo.index_path()).expect("index");
        set_test_inject_index_write_fail_remaining(1);
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("inject checkout failure");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(err, HistoryError::WorktreeApply { .. }));

        let tree_from_index_path = |path: &std::path::Path| {
            let index = crate::index::Index::load(path).expect("index");
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree")
        };
        let before_index_path = dir.path().join(".git/index.before");
        fs::write(&before_index_path, &before_index).expect("snapshot");
        assert_eq!(
            tree_from_index_path(repo.index_path().as_path()),
            tree_from_index_path(&before_index_path)
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );

        let file_path = dir.path().join("file.txt");
        assert!(file_path.is_file());
        assert!(!file_path.is_dir());
        assert_eq!(fs::read_to_string(&file_path).expect("read file"), "file\n");
        assert!(
            !file_path.join("child").exists(),
            "partial file→directory checkout must be rolled back"
        );
    }

    #[test]
    fn snapshot_worktree_for_tree_transition_skips_gitlink_directory() {
        use crate::index::MODE_GITLINK;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let sub_dir = dir.path().join("sub");
        fs::create_dir_all(&sub_dir).expect("mkdir");
        fs::write(sub_dir.join("local.txt"), "submodule local\n").expect("write");

        let sub_tree = repo.odb().write(ObjectKind::Tree, &[]).expect("tree");
        let sub_commit_bytes = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree: sub_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "sub\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let sub_commit = repo
            .odb()
            .write(ObjectKind::Commit, &sub_commit_bytes)
            .expect("commit");

        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_GITLINK,
            uid: 0,
            gid: 0,
            size: 0,
            oid: sub_commit,
            flags: 3,
            flags_extended: None,
            path: b"sub".to_vec(),
            base_index_pos: 0,
        });
        let root_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        let blob2 = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 4,
            oid: blob2,
            flags: 8,
            flags_extended: None,
            path: b"two.txt".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");

        let snap = snapshot_worktree_for_tree_transition(&repo, &root_tree, &child_tree)
            .expect("snapshot");
        assert!(
            !snap.contains_key("sub"),
            "gitlink directories must not be snapshotted for destructive restore"
        );
        assert!(sub_dir.join("local.txt").is_file());
    }

    #[test]
    fn apply_rewrite_checkout_failure_preserves_gitlink_worktree() {
        use crate::index::MODE_GITLINK;
        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let sub_dir = dir.path().join("sub");
        fs::create_dir_all(&sub_dir).expect("mkdir");
        fs::write(sub_dir.join("local.txt"), "submodule local\n").expect("write");

        let sub_tree = repo.odb().write(ObjectKind::Tree, &[]).expect("tree");
        let sub_commit_bytes = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree: sub_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "sub\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let sub_commit = repo
            .odb()
            .write(ObjectKind::Commit, &sub_commit_bytes)
            .expect("commit");

        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_GITLINK,
            uid: 0,
            gid: 0,
            size: 0,
            oid: sub_commit,
            flags: 3,
            flags_extended: None,
            path: b"sub".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let blob2 = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
        let mut idx2 = index.clone();
        idx2.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 4,
            oid: blob2,
            flags: 8,
            flags_extended: None,
            path: b"two.txt".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &idx2, "").expect("tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        set_test_inject_index_write_fail_remaining(1);
        let _err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("inject checkout failure");
        crate::repo::set_test_inject_index_write_fail(false);

        assert!(
            sub_dir.join("local.txt").is_file(),
            "submodule worktree must survive checkout-failure rollback"
        );
        assert!(
            !dir.path().join("two.txt").exists(),
            "partial checkout artifacts must be removed on rollback"
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );
    }

    #[test]
    fn apply_rewrite_checkout_failure_preserves_executable_on_transition_path() {
        #[cfg(not(unix))]
        return;

        use std::os::unix::fs::PermissionsExt;

        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let script_path = dir.path().join("script.sh");
        fs::write(&script_path, b"#!/bin/sh\n").expect("write");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod");

        let blob_v1 = repo
            .odb()
            .write(ObjectKind::Blob, b"#!/bin/sh\n")
            .expect("blob");
        let blob_v2 = repo
            .odb()
            .write(ObjectKind::Blob, b"#!/bin/sh\n# v2\n")
            .expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_EXECUTABLE,
            uid: 0,
            gid: 0,
            size: 11,
            oid: blob_v1,
            flags: 8,
            flags_extended: None,
            path: b"script.sh".to_vec(),
            base_index_pos: 0,
        });
        let root_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree: root_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let mut child_index = index.clone();
        child_index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_EXECUTABLE,
            uid: 0,
            gid: 0,
            size: 11,
            oid: blob_v2,
            flags: 8,
            flags_extended: None,
            path: b"script.sh".to_vec(),
            base_index_pos: 0,
        });
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &child_index, "")
                .expect("child tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");

        set_test_inject_index_write_fail_remaining(1);
        let _err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t> 1 +0000".parse().expect("identity"),
                reflog_message: "rewrite".into(),
                record_operation: None,
                hooks: crate::hooks::PostOperationHooks::Run,
            },
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect_err("checkout must fail");
        crate::repo::set_test_inject_index_write_fail(false);

        let mode = fs::metadata(&script_path)
            .expect("meta")
            .permissions()
            .mode()
            & 0o111;
        assert_ne!(mode, 0, "executable bit must survive failed-apply rollback");
        assert_eq!(fs::read(&script_path).expect("read script"), b"#!/bin/sh\n");
    }
}

#[cfg(test)]
mod ref_update_lines_tests {
    use super::*;
    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    #[test]
    fn ref_update_lines_match_update_ref_stdin_format() {
        let plan = RewritePlan {
            commits: Vec::new(),
            ref_updates: vec![PlannedRefUpdate {
                name: "refs/heads/main".into(),
                old: oid(1),
                new: oid(2),
            }],
            head: Default::default(),
        };
        assert_eq!(
            plan.ref_update_lines(),
            vec![format!(
                "update refs/heads/main {} {}",
                oid(2).to_hex(),
                oid(1).to_hex()
            )]
        );
    }
}
