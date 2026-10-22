//! Structured errors for `--json` output (conflicts, and other typed failures).

use std::error::Error as StdError;
use std::fmt;

use anyhow::Error;
use grit_lib::error::{Error as LibError, OverwriteReason};
use grit_lib::objects::ObjectId;
use schemars::JsonSchema;
use serde::Serialize;

/// Paths that would be overwritten, when `kind == "would_overwrite"`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WouldOverwriteJson {
    /// `local_changes` or `untracked`.
    pub reason: String,
    /// Sorted, deduplicated repository-relative paths.
    pub paths: Vec<String>,
}

/// JSON object emitted on stdout for structured command failures.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StructuredJsonError {
    /// Short, stable summary for scripts.
    pub error: String,
    /// Machine-readable category (e.g. `conflict`).
    pub kind: &'static str,
    /// Conflicting paths, when `kind == "conflict"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
    /// Overwrite details when `kind == "would_overwrite"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub would_overwrite: Option<WouldOverwriteJson>,
    /// Plain SHA-1 digest for colliding input, when `kind == "hash_collision"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
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
            would_overwrite: None,
            oid: None,
        },
        human,
    })
}

/// Build a structured CLI error for SHA-1 chosen-prefix collision detection (sha1dc).
pub fn hash_collision(oid: ObjectId) -> Error {
    let hex = oid.to_hex();
    let human =
        format!("refusing to store content that triggers an SHA-1 hash collision (digest {hex})");
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: "SHA-1 hash collision".to_owned(),
            kind: "hash_collision",
            conflicts: None,
            would_overwrite: None,
            oid: Some(hex),
        },
        human,
    })
}

/// Build a structured CLI error for switch (and similar) overwrite refusal.
pub fn would_overwrite_error(
    reason: OverwriteReason,
    mut paths: Vec<String>,
    human: String,
) -> Error {
    paths.sort();
    paths.dedup();
    let reason_str = reason.to_string();
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: human.clone(),
            kind: "would_overwrite",
            conflicts: None,
            would_overwrite: Some(WouldOverwriteJson {
                reason: reason_str,
                paths,
            }),
            oid: None,
        },
        human,
    })
}

/// Map a library error to `anyhow`, using structured JSON for typed failures.
pub fn map_lib_error(err: LibError) -> Error {
    match err {
        LibError::HashCollision { oid } => hash_collision(oid),
        LibError::WouldOverwrite {
            reason,
            paths,
            message,
        } => would_overwrite_error(reason, paths, message),
        other => Error::new(other),
    }
}

fn hash_collision_from_chain(err: &Error) -> Option<ObjectId> {
    if let Some(LibError::HashCollision { oid }) = err.downcast_ref::<LibError>() {
        return Some(*oid);
    }
    for cause in err.chain() {
        if let Some(LibError::HashCollision { oid }) = cause.downcast_ref::<LibError>() {
            return Some(*oid);
        }
    }
    None
}

/// Wrap [`LibError::HashCollision`] anywhere in the chain as [`StructuredCliError`].
fn would_overwrite_from_chain(err: &Error) -> Option<Error> {
    if let Some(LibError::WouldOverwrite {
        reason,
        paths,
        message,
    }) = err.downcast_ref::<LibError>()
    {
        return Some(would_overwrite_error(
            *reason,
            paths.clone(),
            message.clone(),
        ));
    }
    for cause in err.chain() {
        if let Some(LibError::WouldOverwrite {
            reason,
            paths,
            message,
        }) = cause.downcast_ref::<LibError>()
        {
            return Some(would_overwrite_error(
                *reason,
                paths.clone(),
                message.clone(),
            ));
        }
    }
    None
}

/// Wrap typed library failures anywhere in the chain as [`StructuredCliError`].
pub fn ensure_structured(err: Error) -> Error {
    if structured_json_payload(&err).is_some() {
        return err;
    }
    if let Some(oid) = hash_collision_from_chain(&err) {
        return hash_collision(oid);
    }
    if let Some(structured) = would_overwrite_from_chain(&err) {
        return structured;
    }
    err
}

/// If `err` wraps a [`StructuredCliError`], return its JSON payload.
pub fn structured_json_payload(err: &Error) -> Option<StructuredJsonError> {
    if let Some(e) = err.downcast_ref::<StructuredCliError>() {
        return Some(e.payload.clone());
    }
    hash_collision_from_chain(err).map(|oid| {
        let hex = oid.to_hex();
        StructuredJsonError {
            error: "SHA-1 hash collision".to_owned(),
            kind: "hash_collision",
            conflicts: None,
            would_overwrite: None,
            oid: Some(hex),
        }
    })
}

/// Build a structured error for incompatible `--json` / `--markdown` flag combinations.
pub fn incompatible_output_flags(human: impl Into<String>, kind: &'static str) -> Error {
    let human = human.into();
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: human.clone(),
            kind,
            conflicts: None,
            would_overwrite: None,
            oid: None,
        },
        human,
    })
}

/// Exit code for invalid `--author` / `--grep` patterns (documented on `grit log` / `grit revs`).
pub const INVALID_COMMIT_FILTER_EXIT: i32 = 129;

/// Marker for [`INVALID_COMMIT_FILTER_EXIT`] in the top-level error handler.
#[derive(Debug)]
pub struct InvalidCommitFilterExit;

impl fmt::Display for InvalidCommitFilterExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid commit filter pattern")
    }
}

impl StdError for InvalidCommitFilterExit {}

/// Build a structured CLI error from [`grit_lib::revwalk::filter::CommitFilterError`].
pub fn invalid_commit_filter(err: grit_lib::revwalk::filter::CommitFilterError) -> Error {
    let human = err.to_string();
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: human.clone(),
            kind: "invalid_pattern",
            conflicts: None,
            would_overwrite: None,
            oid: None,
        },
        human,
    })
    .context(InvalidCommitFilterExit)
}

/// Build a structured CLI error from [`grit_lib::diff::PickaxeError`].
pub fn invalid_pickaxe(err: grit_lib::diff::PickaxeError) -> Error {
    let human = err.to_string();
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            error: human.clone(),
            kind: "invalid_pattern",
            conflicts: None,
            would_overwrite: None,
            oid: None,
        },
        human,
    })
    .context(InvalidCommitFilterExit)
}
