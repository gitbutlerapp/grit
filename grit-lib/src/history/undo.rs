//! Undo the last history rewrite from a recorded ref plan.

use crate::hooks::PostOperationHooks;
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

use super::apply::{apply_undo_rewrite, verify_undo_worktree};
use super::error::HistoryError;
use super::last::{read_history_last, HistoryLastRecord};
use super::types::{ApplyOptions, UndoOutcome};
use super::undo_work::{read_undo_work, UndoWork};

/// Revert the last recorded history rewrite when refs are still at the post-rewrite tips.
///
/// Builds the inverse ref plan from [`.git/grit/history-last`](super::last::read_history_last),
/// verifies each ref is still at its recorded `new` OID ([`HistoryError::UndoStale`]), then applies
/// ref updates and the matching worktree transition. On success, writes the inverse as the new
/// record so a second call redoes the original rewrite. Like `reset`, undo runs no
/// post-operation hooks, whatever [`ApplyOptions::hooks`] says.
///
/// Fixup and absorb fold staged (or selected unstaged) edits into history. Undoing them puts those
/// edits back: every consumed path gets the index entry and work tree file it had before the
/// command ([`UndoOutcome::restored_paths`]), and a second undo returns to the post-command
/// state. Records written before grit kept that state still move refs;
/// [`UndoOutcome::unrestored_edits`] then names the commit that holds the edits.
///
/// Refs and `HEAD` are read and moved through the repository's ref store
/// ([`Repository::ref_store`]); the undo record itself is a file under the git directory.
///
/// # Errors
///
/// Returns [`HistoryError::NoUndoRecord`], [`HistoryError::UndoStale`],
/// [`HistoryError::LocalChangesWouldBeOverwritten`] (including a restored path edited since the
/// command), and other apply failures.
pub fn undo_last_rewrite(
    repo: &Repository,
    opts: &ApplyOptions,
    dry_run: bool,
) -> Result<UndoOutcome, HistoryError> {
    undo_last_rewrite_with_cancel(repo, opts, dry_run, &crate::progress::CancelToken::never())
}

/// Like [`undo_last_rewrite`], but stops when `cancel` is set and rolls back partial work.
pub fn undo_last_rewrite_with_cancel(
    repo: &Repository,
    opts: &ApplyOptions,
    dry_run: bool,
    cancel: &crate::progress::CancelToken,
) -> Result<UndoOutcome, HistoryError> {
    let record = read_history_last(repo)?.ok_or(HistoryError::NoUndoRecord)?;
    verify_record_still_valid(repo, &record)?;
    let work = if repo.is_bare() {
        None
    } else {
        read_undo_work(repo)?
    };
    let plan = record.undo_plan();
    let head_change = plan.head_change();
    let restored_paths = work.as_ref().map(UndoWork::paths).unwrap_or_default();
    let unrestored_edits = if work.is_none() && consumed_local_edits(repo, &record) {
        record.head_new_commit
    } else {
        None
    };
    if dry_run {
        verify_undo_worktree(repo, &plan, work.as_ref())?;
        let head_after = head_change
            .map(|(_, new)| new)
            .or(record.head_old_commit)
            .ok_or_else(|| HistoryError::UndoRecordRead {
                message: "record missing head commit for dry-run".into(),
            })?;
        return Ok(UndoOutcome {
            ref_updates: plan.ref_updates.clone(),
            head: head_after,
            head_change,
            restored_paths,
            unrestored_edits,
            dry_run: true,
        });
    }
    let mut apply_opts = opts.clone();
    apply_opts.record_operation = Some("undo".into());
    apply_opts.reflog_message = "history undo".into();
    apply_opts.hooks = PostOperationHooks::Skip;
    let applied = apply_undo_rewrite(repo, &plan, &apply_opts, work.as_ref(), cancel)?;
    Ok(UndoOutcome {
        ref_updates: applied.ref_updates,
        head: applied.head,
        head_change,
        restored_paths,
        unrestored_edits,
        dry_run: false,
    })
}

/// Whether the recorded command folded index or work tree edits into history.
///
/// Fixup and absorb only consume edits when `HEAD`'s tree changes, which is also when they write
/// the sidecar; without one, the record predates it.
fn consumed_local_edits(repo: &Repository, record: &HistoryLastRecord) -> bool {
    !repo.is_bare()
        && matches!(record.operation.as_str(), "fixup" | "absorb")
        && record.head_old_tree != record.head_new_tree
}

fn verify_record_still_valid(
    repo: &Repository,
    record: &HistoryLastRecord,
) -> Result<(), HistoryError> {
    for update in &record.ref_updates {
        let live = repo.references().resolve_if_exists(update.name.as_str());
        if !matches!(live, Ok(Some(oid)) if oid == update.new) {
            return Err(HistoryError::UndoStale {
                name: update.name.as_str().to_owned(),
            });
        }
    }
    if let Some(expected) = record.head_new_commit {
        let head = match resolve_head_for_repository(repo) {
            Ok(state) => state,
            Err(_) => {
                return Err(HistoryError::UndoStale {
                    name: record.head_ref.clone().unwrap_or_else(|| "HEAD".into()),
                })
            }
        };
        let live_commit = match head {
            HeadState::Branch { oid, .. } => oid,
            HeadState::Detached { oid } => Some(oid),
            HeadState::Invalid => None,
        };
        if live_commit != Some(expected) {
            let label = record.head_ref.clone().unwrap_or_else(|| "HEAD".into());
            return Err(HistoryError::UndoStale { name: label });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::types::{HeadEffect, PlannedRefUpdate};
    use crate::objects::ObjectId;
    use crate::refs::store::RefName;

    #[test]
    fn undo_plan_swaps_refs_and_head_trees() {
        let record = HistoryLastRecord {
            operation: "drop".into(),
            head_ref: Some("refs/heads/main".into()),
            head_old_commit: Some(
                ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
            ),
            head_new_commit: Some(
                ObjectId::from_hex("0000000000000000000000000000000000000002").unwrap(),
            ),
            head_old_tree: Some(
                ObjectId::from_hex("0000000000000000000000000000000000000003").unwrap(),
            ),
            head_new_tree: Some(
                ObjectId::from_hex("0000000000000000000000000000000000000004").unwrap(),
            ),
            ref_updates: vec![PlannedRefUpdate {
                name: RefName::from_trusted("refs/heads/main"),
                old: ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap(),
                new: ObjectId::from_hex("0000000000000000000000000000000000000002").unwrap(),
            }],
        };
        let plan = record.undo_plan();
        assert_eq!(plan.ref_updates[0].old, record.ref_updates[0].new);
        assert_eq!(plan.ref_updates[0].new, record.ref_updates[0].old);
        assert_eq!(
            plan.head,
            HeadEffect {
                commit_changes: true,
                old_commit: record.head_new_commit,
                head_ref: record.head_ref.clone(),
                old_tree: record.head_new_tree,
                new_tree: record.head_old_tree,
                checkout_onto: None,
            }
        );
    }
}
