//! Helpers for building [`CommitRequest`] values in integration tests.

use grit_lib::ident::Signature;
use grit_lib::porcelain::commit::CommitRequest;

/// Build a [`CommitRequest`] with the given message and author/committer signature line.
#[must_use]
pub fn commit_request(message: impl Into<String>, ident: &str) -> CommitRequest {
    let sig: Signature = ident.parse().expect("valid signature line");
    let mut req = CommitRequest::new(message);
    req.author = Some(sig.clone());
    req.committer = Some(sig);
    req
}

/// Like [`commit_request`], but allows empty commits.
#[must_use]
pub fn empty_commit_request(message: impl Into<String>, ident: &str) -> CommitRequest {
    let mut req = commit_request(message, ident);
    req.allow_empty = true;
    req
}
