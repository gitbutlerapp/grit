//! Replace a single commit message and/or author and replay descendants.

use crate::ident::{author_identity_line, parse_author_arg};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::commit::normalize_commit_message;
use crate::repo::Repository;

use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::types::{CommitEdit, HistoryEdits, RewriteOptions, RewritePlan};

/// Fields to change when rewriting one commit's message and/or author with
/// [`History::reword`](super::History::reword).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RewordOptions {
    /// Replacement commit message (normalized like [`normalize_rewrite_message`]).
    pub message: Option<String>,
    /// Replacement author (`Name <email>` or bare email); date defaults to the source commit's.
    pub author: Option<String>,
}

impl RewordOptions {
    /// Sets [`message`](Self::message).
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Sets [`author`](Self::author).
    #[must_use]
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }
}

/// Clean a commit message the same way [`crate::porcelain::commit`] does before hooks: stripspace,
/// reject empty bodies, and ensure a trailing newline.
///
/// # Errors
///
/// Returns [`HistoryError::EmptyMessage`] when the cleaned body would be empty.
pub fn normalize_rewrite_message(raw: &str) -> crate::error::Result<String> {
    normalize_commit_message(raw).map_err(|_| HistoryError::EmptyMessage.into())
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
) -> crate::error::Result<String> {
    let contact = parse_author_arg(author_arg)?;
    let date_override = author_date_suffix_from_ident(original_author);
    Ok(author_identity_line(
        &contact,
        date_override.as_deref(),
        repo.identity_now(),
    ))
}

/// Plan a message and/or author rewrite of `commit`; see [`History::reword`](super::History::reword).
pub(crate) fn reword(
    repo: &Repository,
    commit: ObjectId,
    options: &RewordOptions,
    opts: &RewriteOptions,
) -> crate::error::Result<RewritePlan> {
    let has_message = options.message.is_some();
    let has_author = options
        .author
        .as_ref()
        .is_some_and(|a| !a.trim().is_empty());
    if !has_message && !has_author {
        return Err(HistoryError::NothingToReword.into());
    }

    let message = if let Some(ref raw) = options.message {
        Some(normalize_rewrite_message(raw)?)
    } else {
        None
    };

    let author = if let Some(ref arg) = options.author {
        if arg.trim().is_empty() {
            None
        } else {
            let raw = repo.odb().read(&commit)?;
            let parsed = parse_commit(&raw.data)?;
            Some(resolve_reword_author(repo, &parsed.author, arg)?)
        }
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
