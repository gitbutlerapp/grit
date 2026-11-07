//! Drop a single commit and replay descendants onto its parent.

use crate::objects::ObjectId;
use crate::repo::Repository;

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::types::{CommitEdit, HistoryEdits, RewriteOptions, RewritePlan};

/// Plan removal of `commit` from history and replay of its descendants onto the mapped parent.
///
/// See [`History::drop_commit`](super::History::drop_commit).
pub(crate) fn drop(
    repo: &Repository,
    commit: ObjectId,
    opts: &RewriteOptions,
) -> crate::error::Result<RewritePlan> {
    let parsed = super::load_commit(repo, commit)?;
    if parsed.parents.is_empty() {
        return Err(HistoryError::RootCommit { commit }.into());
    }
    if parsed.parents.len() > 1 {
        return Err(HistoryError::MergeCommitInReplay { commit }.into());
    }
    let mut edits = HistoryEdits::new();
    edits.insert(commit, CommitEdit::Drop);
    plan_rewrite(repo, &edits, opts)
}
