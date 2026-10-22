//! Map library and structured errors to short CLI messages.

use anyhow::Error;
use grit_lib::error::Error as LibError;
use grit_lib::refs::store::RefStoreError;

use crate::json_error::StructuredCliError;
use crate::revision_errors::map_lib_revision_error;

/// Human-readable (and JSON `error` string) message for a command failure.
pub fn human_error_message(err: &Error) -> String {
    if let Some(structured) = err.downcast_ref::<StructuredCliError>() {
        return structured.human.clone();
    }
    if let Some(lib) = err.downcast_ref::<LibError>() {
        return map_lib_error(lib);
    }
    for cause in err.chain() {
        if let Some(structured) = cause.downcast_ref::<StructuredCliError>() {
            return structured.human.clone();
        }
        if let Some(LibError::HashCollision { oid }) = cause.downcast_ref::<LibError>() {
            return map_lib_error(&LibError::HashCollision { oid: *oid });
        }
    }
    format!("{err:#}")
}

fn map_lib_error(err: &LibError) -> String {
    if let Some(msg) = map_lib_revision_error(err) {
        return msg;
    }
    match err {
        LibError::HashCollision { oid } => {
            format!(
                "refusing to store content that triggers an SHA-1 hash collision (digest {})",
                oid.to_hex()
            )
        }
        LibError::RefStore(RefStoreError::NotFound { name }) => {
            match name.strip_prefix("refs/heads/") {
                Some(short) => format!("no branch named '{short}'"),
                None => format!("ref not found: {name}"),
            }
        }
        LibError::InvalidPathComponent { path, reason } => {
            format!("cannot use path '{path}': {reason}")
        }
        other => other.git_stderr_message(),
    }
}
