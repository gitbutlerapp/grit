//! Drop a single commit and replay descendants onto its parent.

use crate::objects::{parse_commit, ObjectId};
use crate::repo::Repository;

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::types::{CommitEdit, HistoryEdits, RewriteOptions, RewritePlan};

/// Plan removal of `commit` from history and replay of its descendants onto the mapped parent.
///
/// Writes replacement commit objects into the ODB; refs are updated only when the caller applies
/// the returned [`RewritePlan`].
///
/// # Errors
///
/// Returns [`HistoryError::RootCommit`] when `commit` has no parents, or
/// [`HistoryError::MergeCommitInReplay`] when `commit` is a merge. Other failures come from
/// [`plan_rewrite`].
pub fn drop(
    repo: &Repository,
    commit: ObjectId,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    let raw = repo
        .odb()
        .read(&commit)
        .map_err(|e| HistoryError::Planning {
            message: e.to_string(),
        })?;
    let parsed = parse_commit(&raw.data).map_err(|e| HistoryError::Planning {
        message: e.to_string(),
    })?;
    if parsed.parents.is_empty() {
        return Err(HistoryError::RootCommit { commit });
    }
    if parsed.parents.len() > 1 {
        return Err(HistoryError::MergeCommitInReplay { commit });
    }
    let mut edits = HistoryEdits::new();
    edits.insert(commit, CommitEdit::Drop);
    plan_rewrite(repo, &edits, opts)
}
