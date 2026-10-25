//! Structured errors for `--json` output (conflicts, and other typed failures).

use std::error::Error as StdError;
use std::fmt;

use anyhow::Error;
use grit_lib::command_runner::hooks::HookError;
use grit_lib::error::{Error as LibError, OverwriteReason};
use grit_lib::objects::ObjectId;
use grit_lib::state::{InProgressOperation, NextStep, Operation};
use schemars::JsonSchema;
use serde::Serialize;

use crate::error_hints;
use crate::error_kind::ErrorKind;

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
    pub kind: ErrorKind,
    /// Conflicting paths, when `kind == "conflict"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
    /// Commit that could not be replayed cleanly, when `kind == "conflict"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Overwrite details when `kind == "would_overwrite"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub would_overwrite: Option<WouldOverwriteJson>,
    /// Plain SHA-1 digest for colliding input, when `kind == "hash_collision"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub oid: Option<String>,
    /// The failing hook and what it wrote, when `kind == "hook_failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub hook: Option<HookFailureJson>,
    /// Repository-relative paths the failure is about, when `kind` is `unmerged_paths` or
    /// `dirty_worktree`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// The next command to run, in prose (also printed as `hint:` on stderr).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl StructuredJsonError {
    /// A payload with only `error` and `kind` set.
    #[must_use]
    pub fn new(error: impl Into<String>, kind: ErrorKind) -> Self {
        Self {
            error: error.into(),
            kind,
            conflicts: None,
            commit: None,
            would_overwrite: None,
            oid: None,
            hook: None,
            paths: None,
            hint: None,
        }
    }
}

/// A hook that exited non-zero, when `kind == "hook_failed"`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HookFailureJson {
    /// Hook event name (`pre-commit`, `commit-msg`, …).
    pub name: String,
    /// The hook's exit code.
    pub status: i32,
    /// What the hook wrote to stdout (lossy UTF-8).
    pub stdout: String,
    /// What the hook wrote to stderr (lossy UTF-8).
    pub stderr: String,
}

/// A [`HookError`] anywhere in `err`'s chain, bare or wrapped in [`LibError::Hook`].
pub fn hook_error_from_chain(err: &Error) -> Option<&HookError> {
    err.chain()
        .find_map(|cause| match cause.downcast_ref::<LibError>() {
            Some(LibError::Hook(hook)) => Some(hook),
            _ => cause.downcast_ref::<HookError>(),
        })
}

fn hook_failure_payload(err: &Error) -> Option<StructuredJsonError> {
    let hook = hook_error_from_chain(err)?;
    let HookError::Failed {
        hook_name,
        status,
        output,
    } = hook
    else {
        return None;
    };
    Some(StructuredJsonError {
        hook: Some(HookFailureJson {
            name: hook_name.clone(),
            status: *status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        ..StructuredJsonError::new(hook.to_string(), ErrorKind::HookFailed)
    })
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

/// Build an error for a merge, pick, or rebase that would introduce conflicts.
pub fn operation_conflict(
    operation: &str,
    mut paths: Vec<String>,
    commit: Option<ObjectId>,
) -> Error {
    paths.sort();
    paths.dedup();
    let list = paths.join("\n  ");
    let commit_line = commit
        .as_ref()
        .map(|oid| format!("\n  commit {}", &oid.to_hex()[..7.min(oid.to_hex().len())]))
        .unwrap_or_default();
    let human = format!(
        "{operation} has conflicts in:{commit_line}\n  {list}\n\nNothing was changed. grit can't resolve conflicts yet."
    );
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            conflicts: Some(paths),
            commit: commit.map(|oid| oid.to_hex()),
            ..StructuredJsonError::new(format!("{operation} has conflicts"), ErrorKind::Conflict)
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
            oid: Some(hex),
            ..StructuredJsonError::new("SHA-1 hash collision", ErrorKind::HashCollision)
        },
        human,
    })
}

/// Build a structured CLI error for switch (and similar) overwrite refusal.
///
/// `message` is the library summary; the human text adds the paths and both get a hint.
pub fn would_overwrite_error(
    reason: OverwriteReason,
    mut paths: Vec<String>,
    message: String,
) -> Error {
    paths.sort();
    paths.dedup();
    let human = format!("{message}{}", error_hints::indented_paths(&paths));
    Error::new(StructuredCliError {
        payload: StructuredJsonError {
            hint: Some(error_hints::would_overwrite_hint(reason).to_owned()),
            would_overwrite: Some(WouldOverwriteJson {
                reason: reason.to_string(),
                paths,
            }),
            ..StructuredJsonError::new(message, ErrorKind::WouldOverwrite)
        },
        human,
    })
}

/// Build a structured CLI error for [`LibError::IndexLocked`] (`human` is the library message).
pub fn index_locked(human: String) -> Error {
    Error::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), ErrorKind::IndexLocked),
        human,
    })
}

/// Build a structured CLI error for a command refused while `kind` is paused.
///
/// `operation` supplies the finish / cancel commands named in the message, when grit knows them.
pub fn operation_in_progress(
    action: &str,
    kind: InProgressOperation,
    operation: Option<&Operation>,
) -> Error {
    let description = kind.description();
    let article = if description.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    let mut human = format!("cannot {action} while {article} {description} is in progress");
    if let Some(op) = operation {
        let finish = op.next_steps.iter().rev().find_map(NextStep::command);
        let cancel = op.alternatives.iter().find_map(NextStep::command);
        match (finish, cancel) {
            (Some(finish), Some(cancel)) => {
                human.push_str(&format!(
                    "; finish it with `{finish}` or cancel it with `{cancel}`"
                ));
            }
            (Some(cmd), None) => human.push_str(&format!("; finish it with `{cmd}`")),
            (None, Some(cmd)) => human.push_str(&format!("; cancel it with `{cmd}`")),
            (None, None) => {}
        }
    }
    Error::new(StructuredCliError {
        payload: StructuredJsonError::new(
            format!("{description} in progress"),
            ErrorKind::OperationInProgress,
        ),
        human,
    })
}

/// Build a structured CLI error for a revision that names nothing.
pub fn revision_not_found(spec: &str) -> Error {
    let human = format!("no branch, tag or commit named '{spec}'");
    Error::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), ErrorKind::RevisionNotFound),
        human,
    })
}

/// Map a library error to `anyhow`, using structured JSON for typed failures.
pub fn map_lib_error(err: LibError) -> Error {
    match err {
        LibError::HashCollision { oid } => hash_collision(oid),
        locked @ LibError::IndexLocked { .. } => index_locked(locked.to_string()),
        LibError::RevisionNotFound(spec) => revision_not_found(&spec),
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
    if let Some(locked) = err
        .chain()
        .find_map(|cause| match cause.downcast_ref::<LibError>() {
            Some(locked @ LibError::IndexLocked { .. }) => Some(locked.to_string()),
            _ => None,
        })
    {
        return index_locked(locked);
    }
    error_hints::guided_error(&err).unwrap_or(err)
}

/// If `err` wraps a [`StructuredCliError`], return its JSON payload.
pub fn structured_json_payload(err: &Error) -> Option<StructuredJsonError> {
    if let Some(e) = err.downcast_ref::<StructuredCliError>() {
        return Some(e.payload.clone());
    }
    if let Some(payload) = hook_failure_payload(err) {
        return Some(payload);
    }
    hash_collision_from_chain(err).map(|oid| StructuredJsonError {
        oid: Some(oid.to_hex()),
        ..StructuredJsonError::new("SHA-1 hash collision", ErrorKind::HashCollision)
    })
}

/// Build a structured error for incompatible `--json` / `--markdown` flag combinations.
pub fn incompatible_output_flags(human: impl Into<String>, kind: ErrorKind) -> Error {
    let human = human.into();
    Error::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), kind),
        human,
    })
}

/// Marker for invalid commit filter exit (129) in the top-level error handler.
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
        payload: StructuredJsonError::new(human.clone(), ErrorKind::InvalidPattern),
        human,
    })
    .context(InvalidCommitFilterExit)
}

/// Build a structured CLI error from [`grit_lib::diff::PickaxeError`].
pub fn invalid_pickaxe(err: grit_lib::diff::PickaxeError) -> Error {
    let human = err.to_string();
    Error::new(StructuredCliError {
        payload: StructuredJsonError::new(human.clone(), ErrorKind::InvalidPattern),
        human,
    })
    .context(InvalidCommitFilterExit)
}
