//! Replace a single commit message and replay descendants.

use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::stripspace::{process, Mode};

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::types::{CommitEdit, HistoryEdits, RewriteOptions, RewritePlan};

/// Clean a commit message the same way [`crate::porcelain::commit`] does before hooks: stripspace,
/// reject empty bodies, and ensure a trailing newline.
///
/// # Errors
///
/// Returns [`HistoryError::EmptyMessage`] when the cleaned body would be empty.
pub fn normalize_rewrite_message(raw: &str) -> Result<String, HistoryError> {
    let trimmed = raw.trim();
    let processed = process(trimmed.as_bytes(), &Mode::Default);
    let message = String::from_utf8(processed).map_err(|_| HistoryError::EmptyMessage)?;
    if message.is_empty() {
        return Err(HistoryError::EmptyMessage);
    }
    let mut message = message;
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(message)
}

/// Plan a message-only rewrite of `commit` and replay of its descendants.
///
/// Writes replacement commit objects into the ODB; refs are updated only when the caller applies
/// the returned [`RewritePlan`].
///
/// # Errors
///
/// Returns [`HistoryError::EmptyMessage`] or any error from [`plan_rewrite`].
pub fn reword(
    repo: &Repository,
    commit: ObjectId,
    message: String,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    let message = normalize_rewrite_message(&message)?;
    let mut edits = HistoryEdits::new();
    edits.insert(
        commit,
        CommitEdit::Rewrite {
            tree: None,
            message: Some(message),
            author: None,
            parents: None,
        },
    );
    plan_rewrite(repo, &edits, opts)
}
