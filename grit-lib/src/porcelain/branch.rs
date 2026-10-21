//! Local branches: create one without switching `HEAD`, list them, and delete one.

use crate::branch_tracking::upstream_branch;
use crate::check_ref_format::validate_branch_short_name;
use crate::error::{Error, Result};
use crate::merge_base::is_ancestor;
use crate::objects::ObjectId;
use crate::porcelain::checkout_reflog::{
    branch_create_reflog_source_label, create_branch_ref_with_reflog,
};
use crate::refs;
use crate::repo::Repository;
use crate::state::{resolve_head, resolve_head_for_repository, HeadState};
use crate::worktree::ensure_branch_not_checked_out_in_any_worktree;

const HEADS_PREFIX: &str = "refs/heads/";

/// Outcome of [`create_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBranchOutcome {
    /// Short branch name.
    pub name: String,
    /// Tip commit the new branch points at.
    pub oid: ObjectId,
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
    let branch_ref = format!("{HEADS_PREFIX}{name}");
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

/// One local branch in a [`BranchList`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBranch {
    /// Short branch name (without `refs/heads/`).
    pub name: String,
    /// Commit the branch points at.
    pub oid: ObjectId,
    /// Whether `HEAD` is attached to this branch.
    pub current: bool,
}

/// Result of [`list_branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchList {
    /// Branch `HEAD` is attached to, or `None` when `HEAD` is detached or invalid.
    ///
    /// Set even when the branch is unborn, in which case it is absent from
    /// [`branches`](Self::branches).
    pub current: Option<String>,
    /// Every local branch, sorted by name.
    pub branches: Vec<LocalBranch>,
}

/// List the local branches (`refs/heads/*`) of `repo`, sorted by name.
///
/// Reads through the repository's ref store, so the reftable backend and
/// linked-worktree ref visibility are honoured.
///
/// # Errors
///
/// Returns an error when `HEAD` or the ref store cannot be read.
pub fn list_branches(repo: &Repository) -> Result<BranchList> {
    let current = match resolve_head_for_repository(repo)? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        HeadState::Detached { .. } | HeadState::Invalid => None,
    };
    let branches = repo
        .references()
        .list(HEADS_PREFIX)?
        .into_iter()
        .map(|(refname, oid)| {
            let name = refname
                .strip_prefix(HEADS_PREFIX)
                .unwrap_or(&refname)
                .to_owned();
            let current = current.as_deref() == Some(name.as_str());
            LocalBranch { name, oid, current }
        })
        .collect();
    Ok(BranchList { current, branches })
}

/// Result of [`delete_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedBranch {
    /// Short branch name that was deleted.
    pub name: String,
    /// Commit the branch pointed at before deletion.
    pub oid: ObjectId,
}

/// Delete `refs/heads/<name>` and its reflog through the repository's ref store.
///
/// Unless `force` is set, the branch tip must be reachable from the branch's
/// upstream when it has one that resolves, otherwise from `HEAD`. A branch with
/// neither (for example while `HEAD` is unborn) only deletes with `force`.
///
/// # Parameters
///
/// - `repo`: the repository.
/// - `name`: short branch name (without `refs/heads/`).
/// - `force`: skip the merged check.
///
/// # Errors
///
/// - [`Error::CannotDeleteCurrentBranch`] when `HEAD` is attached to `name`.
/// - [`Error::BranchCheckedOutInWorktree`] when another worktree has it checked out.
/// - [`Error::BranchNotFound`] when the branch does not exist.
/// - [`Error::BranchNotMerged`] when `force` is unset and the tip is not merged.
/// - Ref store errors (including a compare-and-swap failure when the branch moved after it
///   was checked), object database and ancestry walk errors are propagated.
pub fn delete_branch(repo: &Repository, name: &str, force: bool) -> Result<DeletedBranch> {
    let head = resolve_head_for_repository(repo)?;
    if matches!(&head, HeadState::Branch { short_name, .. } if short_name == name) {
        return Err(Error::CannotDeleteCurrentBranch {
            branch: name.to_owned(),
        });
    }
    ensure_branch_not_checked_out_in_any_worktree(repo, name)?;

    let branch_ref = format!("{HEADS_PREFIX}{name}");
    let existing = repo
        .references()
        .read_existing(&branch_ref)?
        .ok_or_else(|| Error::BranchNotFound {
            branch: name.to_owned(),
        })?;
    let oid = existing.oid;

    if !force && !is_merged(repo, name, oid, &head)? {
        return Err(Error::BranchNotMerged {
            branch: name.to_owned(),
        });
    }

    repo.references().delete_verified(&branch_ref, &existing)?;
    Ok(DeletedBranch {
        name: name.to_owned(),
        oid,
    })
}

/// Whether `tip` is reachable from the merge base used by [`delete_branch`].
fn is_merged(repo: &Repository, name: &str, tip: ObjectId, head: &HeadState) -> Result<bool> {
    let base = upstream_branch(repo, name)
        .map(|upstream| upstream.oid)
        .or_else(|| head.oid().copied());
    match base {
        Some(base) => is_ancestor(repo, tip, base),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::repo::InitOptions;

    fn init() -> (tempfile::TempDir, Repository) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(tmp.path(), &InitOptions::default()).expect("init");
        (tmp, repo)
    }

    fn commit(repo: &Repository, message: &str) -> ObjectId {
        let req = CommitRequest {
            message: message.into(),
            author: "T <t@example.com> 1700000000 +0000".into(),
            committer: "T <t@example.com> 1700000000 +0000".into(),
            allow_empty: true,
            sign_override: None,
            amend: false,
            assume_worktree_synced: false,
            skip_pre_commit_tree_match: false,
            staged_change_count: None,
            hooks: Default::default(),
        };
        create_commit(repo, &req, &mut NullProgress)
            .expect("commit")
            .oid
    }

    fn branch_exists(repo: &Repository, name: &str) -> bool {
        repo.references()
            .resolve_if_exists(&format!("{HEADS_PREFIX}{name}"))
            .unwrap()
            .is_some()
    }

    fn current(repo: &Repository) -> String {
        list_branches(repo)
            .expect("list")
            .current
            .expect("on a branch")
    }

    #[test]
    fn list_on_unborn_head_reports_current_without_branches() {
        let (_tmp, repo) = init();
        let list = list_branches(&repo).unwrap();
        assert!(list.current.is_some());
        assert!(list.branches.is_empty());
    }

    #[test]
    fn list_marks_current_and_sorts_by_name() {
        let (_tmp, repo) = init();
        let tip = commit(&repo, "one");
        create_branch(&repo, "zeta").unwrap();
        create_branch(&repo, "alpha").unwrap();
        let list = list_branches(&repo).unwrap();
        let current = list.current.clone().unwrap();

        let names: Vec<&str> = list.branches.iter().map(|b| b.name.as_str()).collect();
        let mut expected = vec!["alpha", "zeta", current.as_str()];
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(list.branches.iter().all(|b| b.oid == tip));
        for branch in &list.branches {
            assert_eq!(branch.current, branch.name == current, "{branch:?}");
        }
    }

    #[test]
    fn delete_refuses_current_branch_even_with_force() {
        let (_tmp, repo) = init();
        commit(&repo, "one");
        let current = current(&repo);
        let err = delete_branch(&repo, &current, true).unwrap_err();
        assert!(
            matches!(&err, Error::CannotDeleteCurrentBranch { branch } if *branch == current),
            "{err:?}"
        );
    }

    #[test]
    fn delete_missing_branch_is_typed() {
        let (_tmp, repo) = init();
        commit(&repo, "one");
        let err = delete_branch(&repo, "nope", true).unwrap_err();
        assert!(
            matches!(&err, Error::BranchNotFound { branch } if branch == "nope"),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "no branch named 'nope'");
    }

    #[test]
    fn delete_merged_branch_returns_old_tip_and_removes_ref() {
        let (_tmp, repo) = init();
        let tip = commit(&repo, "one");
        create_branch(&repo, "topic").unwrap();
        commit(&repo, "two");

        let deleted = delete_branch(&repo, "topic", false).unwrap();
        assert_eq!(
            deleted,
            DeletedBranch {
                name: "topic".into(),
                oid: tip,
            }
        );
        assert!(!branch_exists(&repo, "topic"));
    }

    #[test]
    fn delete_unmerged_branch_requires_force() {
        let (_tmp, repo) = init();
        let first = commit(&repo, "one");
        let second = commit(&repo, "two");
        create_branch(&repo, "topic").unwrap();
        let current = current(&repo);
        refs::write_ref(repo.git_dir(), &format!("{HEADS_PREFIX}{current}"), &first).unwrap();

        let err = delete_branch(&repo, "topic", false).unwrap_err();
        assert!(
            matches!(&err, Error::BranchNotMerged { branch } if branch == "topic"),
            "{err:?}"
        );
        assert!(branch_exists(&repo, "topic"));

        let deleted = delete_branch(&repo, "topic", true).unwrap();
        assert_eq!(deleted.oid, second);
        assert!(!branch_exists(&repo, "topic"));
    }
}
