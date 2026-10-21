//! Map library and structured errors to short CLI messages.

use anyhow::Error;
use grit_lib::error::Error as LibError;
use grit_lib::refs::store::RefStoreError;
use grit_lib::revwalk::rev_parse_error::RevParseError;

use crate::json_error::StructuredCliError;

/// Human-readable (and JSON `error` string) message for a command failure.
pub fn human_error_message(err: &Error) -> String {
    if let Some(structured) = err.downcast_ref::<StructuredCliError>() {
        return structured.human.clone();
    }
    if let Some(lib) = err.downcast_ref::<LibError>() {
        return map_lib_error(lib);
    }
    format!("{err:#}")
}

fn map_lib_error(err: &LibError) -> String {
    match err {
        LibError::RevParse(RevParseError::InvalidObjectName { name }) => {
            format!("revision '{name}' not found")
        }
        LibError::RevParse(RevParseError::AmbiguousArgument { spec }) => {
            format!("revision '{spec}' not found")
        }
        LibError::ObjectNotFound(spec) => format!("revision '{spec}' not found"),
        LibError::RefStore(RefStoreError::NotFound { name }) => {
            match name.strip_prefix("refs/heads/") {
                Some(short) => format!("no branch named '{short}'"),
                None => format!("ref not found: {name}"),
            }
        }
        other => other.git_stderr_message(),
    }
}
