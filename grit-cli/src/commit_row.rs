//! Commit one-line summaries for log-style listings (no repository handle).

use grit_lib::objects::ObjectId;

/// A one-line summary of a commit, for shortlog-style output.
#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub oid: ObjectId,
    pub subject: String,
    /// Author display name: the identity name when present, otherwise the email local-part.
    pub author: String,
    /// Author date as a Unix timestamp (for relative-date rendering).
    pub timestamp: i64,
    /// Full reverse-hex change id for display.
    pub change_id: String,
}
