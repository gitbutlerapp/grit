//! Test helper mirroring the old optional discovery semantics.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, NotARepositoryReason, Result};
use grit_lib::repo::{Repository, RepositoryError};

pub fn discover_optional(start: Option<&Path>) -> Result<Option<Repository>> {
    let err = match Repository::discover(start, &RepositoryOptions::empty()) {
        Ok(repo) => return Ok(Some(repo)),
        Err(err) => err,
    };
    let Error::Repository(RepositoryError::NotARepository(reason)) = &err else {
        return Err(err);
    };
    let malformed_gitfile = matches!(
        reason,
        NotARepositoryReason::GitfileNotRegularFile { .. }
            | NotARepositoryReason::GitfileMissingGitdir { .. }
    );
    let dot_git_at_start = start.is_some_and(|start| {
        let dot_git = start.join(".git");
        dot_git.is_file() || dot_git.is_symlink()
    });
    if malformed_gitfile || dot_git_at_start {
        return Err(err);
    }
    Ok(None)
}
