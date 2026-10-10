//! Structured errors for `--json` output (conflicts, and other typed failures).

use std::error::Error as StdError;
use std::fmt;

use anyhow::Error;
use serde::Serialize;

/// JSON object emitted on stdout for structured command failures.
#[derive(Debug, Clone, Serialize)]
pub struct StructuredJsonError {
    /// Short, stable summary for scripts.
    pub error: String,
    /// Machine-readable category (e.g. `conflict`).
    pub kind: &'static str,
    /// Conflicting paths, when `kind == "conflict"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
}

/// Carries both human text and a structured JSON payload.
#[derive(Debug)]
pub struct StructuredCliError {
    pub payload: StructuredJsonError,
    pub human: String,
}

impl fmt::Display for StructuredCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.human)
    }
}

impl StdError for StructuredCliError {}

/// Build an error for a merge or pick that would introduce conflicts.
pub fn operation_conflict(operation: &str, mut paths: Vec<String>) -> Error {
    paths.sort();
    paths.dedup();
    let list = paths.join("\n  ");
    let human = format!(
        "{operation} has conflicts in:\n  {list}\n\nNothing was changed. grit can't resolve conflicts yet."
    );
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: format!("{operation} has conflicts"),
            kind: "conflict",
            conflicts: Some(paths),
        },
        human,
    })
}

/// If `err` wraps a [`StructuredCliError`], return its JSON payload.
pub fn structured_json_payload(err: &Error) -> Option<StructuredJsonError> {
    err.downcast_ref::<StructuredCliError>()
        .map(|e| e.payload.clone())
}
