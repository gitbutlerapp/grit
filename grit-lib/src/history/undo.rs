//! Undo the last history rewrite from a recorded ref plan.

use crate::hooks::PostOperationHooks;
use crate::refs::resolve_ref;
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

use super::apply::{apply_rewrite, verify_rewrite_plan};
use super::error::HistoryError;
use super::last::{read_history_last, HistoryLastRecord};
use super::types::{ApplyOptions, UndoOutcome};

/// Revert the last recorded history rewrite when refs are still at the post-rewrite tips.
///
/// Builds the inverse ref plan from [`.git/grit/history-last`](super::last::read_history_last),
/// verifies each ref is still at its recorded `new` OID ([`HistoryError::UndoStale`]), then applies
/// ref updates and the matching worktree transition. On success, writes the inverse as the new
/// record so a second call redoes the original rewrite. Like `reset`, undo runs no
/// post-operation hooks, whatever [`ApplyOptions::hooks`] says.
///
/// # Errors
///
/// Returns [`HistoryError::NoUndoRecord`], [`HistoryError::UndoStale`],
/// [`HistoryError::LocalChangesWouldBeOverwritten`], and other apply failures.
pub fn undo_last_rewrite(
    repo: &Repository,
    opts: &ApplyOptions,
    dry_run: bool,
) -> Result<UndoOutcome, HistoryError> {
    let record = read_history_last(repo)?.ok_or(HistoryError::NoUndoRecord)?;
    verify_record_still_valid(repo, &record)?;
    let plan = record.undo_plan();
    verify_rewrite_plan(repo, &plan)?;
    let head_change = plan.head_change();
    if dry_run {
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
            dry_run: true,
        });
    }
    let mut apply_opts = opts.clone();
    apply_opts.record_operation = Some("undo".into());
    apply_opts.reflog_message = "history undo".into();
    apply_opts.hooks = PostOperationHooks::Skip;
    let applied = apply_rewrite(
        repo,
        &plan,
        &apply_opts,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )?;
    Ok(UndoOutcome {
        ref_updates: applied.ref_updates,
        head: applied.head,
        head_change,
        dry_run: false,
    })
}

fn verify_record_still_valid(
    repo: &Repository,
    record: &HistoryLastRecord,
) -> Result<(), HistoryError> {
    for update in &record.ref_updates {
        let live = match resolve_ref(repo.git_dir(), &update.name) {
            Ok(oid) => oid,
            Err(_) => {
                return Err(HistoryError::UndoStale {
                    name: update.name.clone(),
                })
            }
        };
        if live != update.new {
            return Err(HistoryError::UndoStale {
                name: update.name.clone(),
            });
        }
    }
    if let Some(expected) = record.head_new_commit {
        let head = match resolve_head(repo.git_dir()) {
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
                name: "refs/heads/main".into(),
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
