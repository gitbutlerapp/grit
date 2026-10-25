//! Structured errors when `HEAD` is on a branch with no commits yet.

use anyhow::Error;
use grit_lib::error::Error as LibError;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head_for_repository, HeadState};

use crate::context;
use crate::error_kind::ErrorKind;
use crate::json_error::{StructuredCliError, StructuredJsonError};

/// Human and JSON error for an unborn branch, optionally naming the branch.
#[must_use]
pub fn unborn_head_error(branch: Option<&str>) -> Error {
    let human = match branch {
        Some(name) => {
            format!(r#"no commits yet on branch {name} — make one with `grit commit "message"`"#)
        }
        None => {
            r#"no commits yet on this branch — make one with `grit commit "message"`"#.to_owned()
        }
    };
    let hint = Some(r#"make one with `grit commit "message"`"#.to_owned());
    Error::new(StructuredCliError {
        human: human.clone(),
        payload: StructuredJsonError {
            hint,
            ..StructuredJsonError::new(human, ErrorKind::BranchNoCommits)
        },
    })
}

/// When the current repository has an unborn `HEAD`, return a guided error.
pub fn unborn_head_from_cwd() -> Option<Error> {
    let repo = context::discover().ok()?;
    unborn_head_from_repo(&repo)
}

/// When `repo` has an unborn branch at `HEAD`, return a guided error.
pub fn unborn_head_from_repo(repo: &Repository) -> Option<Error> {
    match resolve_head_for_repository(repo).ok()? {
        HeadState::Branch {
            short_name,
            oid: None,
            ..
        } => Some(unborn_head_error(Some(&short_name))),
        _ => None,
    }
}

/// Replace bare [`LibError::UnbornHead`] anywhere in the chain with a guided error.
pub fn enrich_unborn_head(err: Error) -> Error {
    if err.downcast_ref::<StructuredCliError>().is_some() {
        return err;
    }
    let has_unborn = err
        .chain()
        .any(|cause| matches!(cause.downcast_ref::<LibError>(), Some(LibError::UnbornHead)));
    if !has_unborn {
        return err;
    }
    unborn_head_from_cwd().unwrap_or_else(|| unborn_head_error(None))
}
