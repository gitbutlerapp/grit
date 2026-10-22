//! Replace a single commit message and/or author and replay descendants.

use crate::ident::{author_identity_line, parse_author_arg};
use crate::objects::{parse_commit, ObjectId};
use crate::repo::Repository;
use crate::stripspace::{process, Mode};

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::types::{CommitEdit, HistoryEdits, RewriteOptions, RewritePlan};

/// Fields to change when rewriting one commit's message and/or author.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewordOptions {
    /// Replacement commit message (normalized like [`normalize_rewrite_message`]).
    pub message: Option<String>,
    /// Replacement author (`Name <email>` or bare email); date defaults to the source commit's.
    pub author: Option<String>,
}

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

fn author_date_suffix_from_ident(ident: &str) -> Option<String> {
    let rest = ident.rfind('>').map(|i| ident[i + 1..].trim())?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

fn resolve_reword_author(
    repo: &Repository,
    original_author: &str,
    author_arg: &str,
) -> Result<String, HistoryError> {
    let contact = parse_author_arg(author_arg)?;
    let date_override = author_date_suffix_from_ident(original_author);
    Ok(author_identity_line(
        &contact,
        date_override.as_deref(),
        repo.identity_now(),
    ))
}

/// Plan a message and/or author rewrite of `commit` and replay of its descendants.
///
/// Writes replacement commit objects into the ODB; refs are updated only when the caller applies
/// the returned [`RewritePlan`].
///
/// # Errors
///
/// Returns [`HistoryError::NothingToReword`] when both options are absent,
/// [`HistoryError::EmptyMessage`] when a provided message is empty after cleanup,
/// [`HistoryError::InvalidAuthor`] when the author string is invalid, or any error from
/// [`plan_rewrite`].
pub fn reword_with(
    repo: &Repository,
    commit: ObjectId,
    options: &RewordOptions,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    let has_message = options.message.is_some();
    let has_author = options
        .author
        .as_ref()
        .is_some_and(|a| !a.trim().is_empty());
    if !has_message && !has_author {
        return Err(HistoryError::NothingToReword);
    }

    let message = if let Some(ref raw) = options.message {
        Some(normalize_rewrite_message(raw)?)
    } else {
        None
    };

    let author = if let Some(ref arg) = options.author {
        if arg.trim().is_empty() {
            return Err(HistoryError::NothingToReword);
        }
        let raw = repo
            .odb()
            .read(&commit)
            .map_err(|e| HistoryError::Planning {
                message: e.to_string(),
            })?;
        let parsed = parse_commit(&raw.data).map_err(|e| HistoryError::Planning {
            message: e.to_string(),
        })?;
        Some(resolve_reword_author(repo, &parsed.author, arg)?)
    } else {
        None
    };

    let mut edits = HistoryEdits::new();
    edits.insert(
        commit,
        CommitEdit::Rewrite {
            tree: None,
            message,
            author,
            parents: None,
        },
    );
    plan_rewrite(repo, &edits, opts)
}

/// Plan a message-only rewrite of `commit` and replay of its descendants.
///
/// Writes replacement commit objects into the ODB; refs are updated only when the caller applies
/// the returned [`RewritePlan`].
///
/// # Errors
///
/// Returns [`HistoryError::EmptyMessage`] or any error from [`reword_with`].
pub fn reword(
    repo: &Repository,
    commit: ObjectId,
    message: String,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    reword_with(
        repo,
        commit,
        &RewordOptions {
            message: Some(message),
            author: None,
        },
        opts,
    )
}
