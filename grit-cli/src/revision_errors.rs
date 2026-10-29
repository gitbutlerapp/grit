//! Shared revision resolution error text for commands.

use anyhow::Error as AnyhowError;
use grit_lib::error::Error as LibError;
use grit_lib::object_inspect::ResolveError;
use grit_lib::revwalk::rev_parse_error::{AmbiguousChangeIdHint, RevParseError};

use crate::json_error;

/// Format an ambiguous change-id error with candidate rows (oid, short id, subject).
#[must_use]
pub fn format_ambiguous_change_id(spec: &str, candidates: &[AmbiguousChangeIdHint]) -> String {
    let mut msg = format!("ambiguous change id '{spec}'");
    for hint in candidates {
        let short_id = hint.change_id.chars().take(8).collect::<String>();
        msg.push_str(&format!(
            "\n  {} {} {}",
            hint.oid_hex, short_id, hint.subject
        ));
    }
    msg
}

/// Map a library [`LibError`] from revision parsing to a user-facing message.
#[must_use]
pub fn map_lib_revision_error(err: &LibError) -> Option<String> {
    match err {
        LibError::RevParse(RevParseError::AmbiguousChangeId { prefix, candidates }) => {
            Some(format_ambiguous_change_id(prefix, candidates))
        }
        LibError::RevParse(RevParseError::InvalidObjectName { name }) => {
            Some(format!("revision '{name}' not found"))
        }
        LibError::RevParse(RevParseError::AmbiguousArgument { spec }) => {
            Some(format!("revision '{spec}' not found"))
        }
        LibError::RevParse(RevParseError::ReflogDateUnparseable { date }) => {
            Some(format!("invalid reflog date '{date}'"))
        }
        LibError::ObjectNotFound(spec) => Some(format!("revision '{spec}' not found")),
        _ => None,
    }
}

/// Map an error from expanding commit arguments (`grit pick` / `grit revert`) to a user-facing error.
#[must_use]
pub fn map_commit_spec_error(err: LibError) -> AnyhowError {
    if let Some(msg) = map_lib_revision_error(&err) {
        return json_error::structured_revision_not_found(msg);
    }
    match err {
        LibError::InvalidRef(msg) => AnyhowError::msg(msg),
        LibError::ObjectNotFound(msg) => {
            AnyhowError::msg(format!("could not resolve commit: {msg}"))
        }
        other => AnyhowError::new(other),
    }
}

/// Map [`ResolveError`] from [`Repository::resolve`](grit_lib::repo::Repository::resolve).
#[must_use]
pub fn map_resolve_error(spec: &str, err: ResolveError) -> AnyhowError {
    match err {
        ResolveError::AmbiguousShortObject { candidates, .. } => {
            let mut msg = format!("ambiguous short object id '{spec}'");
            for hint in candidates {
                let kind = hint.kind.unwrap_or("object");
                msg.push_str(&format!("\n  {} {}", kind, hint.oid_hex));
            }
            anyhow::anyhow!(msg)
        }
        ResolveError::AmbiguousChangeId { candidates, .. } => {
            anyhow::anyhow!(format_ambiguous_change_id(spec, &candidates))
        }
        ResolveError::PathNotInTree { path, revision } => {
            anyhow::anyhow!("path '{path}' does not exist in '{revision}'")
        }
        ResolveError::ReflogDateUnparseable { date } => {
            anyhow::anyhow!("invalid reflog date '{date}'")
        }
        ResolveError::UnknownRevision { .. } => {
            json_error::structured_revision_not_found(format!("unknown revision '{spec}'"))
        }
        ResolveError::RevisionRange { .. } => {
            anyhow::anyhow!("revision range or set '{spec}' is not a single object")
        }
        ResolveError::WrongObjectKind {
            found, expected, ..
        } => anyhow::anyhow!(
            "object '{spec}' is a {}, not a {}",
            object_kind_label(found),
            object_kind_label(expected)
        ),
        _ => anyhow::anyhow!("could not resolve '{spec}'"),
    }
}

fn object_kind_label(kind: grit_lib::objects::ObjectKind) -> &'static str {
    use grit_lib::objects::ObjectKind;
    match kind {
        ObjectKind::Blob => "blob",
        ObjectKind::Tree => "tree",
        ObjectKind::Commit => "commit",
        ObjectKind::Tag => "tag",
    }
}
