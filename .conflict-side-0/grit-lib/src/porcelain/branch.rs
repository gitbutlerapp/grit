//! Create a local branch without switching `HEAD`.

use crate::check_ref_format::validate_branch_short_name;
use crate::error::{Error, Result};
use crate::porcelain::checkout_reflog::{
    branch_create_reflog_source_label, create_branch_ref_with_reflog,
};
use crate::refs;
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

/// Outcome of [`create_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBranchOutcome {
    /// Short branch name.
    pub name: String,
    /// Tip commit the new branch points at.
    pub oid: crate::objects::ObjectId,
}

/// Create `refs/heads/<name>` at the current `HEAD` commit without checking it out.
///
/// Writes a `branch: Created from …` reflog entry when autocreate is enabled.
///
/// # Errors
///
/// Returns an error when the name is invalid, the branch exists, or there is no commit yet.
pub fn create_branch(repo: &Repository, name: &str) -> Result<CreateBranchOutcome> {
    validate_branch_short_name(name)
        .map_err(|e| Error::Message(format!("invalid branch name '{name}': {e}")))?;
    let branch_ref = format!("refs/heads/{name}");
    if refs::resolve_ref(repo.git_dir(), &branch_ref).is_ok() {
        return Err(Error::Message(format!("branch '{name}' already exists")));
    }

    let head = resolve_head(repo.git_dir())?;
    let base = match head {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        HeadState::Branch { .. } => {
            return Err(Error::Message(
                "no commits yet to create a branch from".into(),
            ));
        }
        HeadState::Invalid => return Err(Error::Message("HEAD is in an unknown state".into())),
    };

    let source = branch_create_reflog_source_label(&head);
    create_branch_ref_with_reflog(repo, &branch_ref, base, &source)?;

    Ok(CreateBranchOutcome {
        name: name.to_owned(),
        oid: base,
    })
}
