//! Local branches: create one without switching `HEAD`, list them, delete one, and rename one.

use std::fs;
use std::ops::ControlFlow;

use crate::branch_tracking::upstream_branch;
use crate::check_ref_format::validate_branch_short_name;
use crate::config::{ConfigFile, ConfigScope, ConfigSet};
use crate::error::{Error, Result};
use crate::merge_base::is_ancestor;
use crate::objects::ObjectId;
use crate::porcelain::checkout_reflog::{
    branch_create_reflog_source_label, checkout_reflog_identity, create_branch_ref_with_reflog,
};
use crate::refs;
use crate::refs::store::{Expected, RawRef, RefUpdate, RefUpdateFlags};
use crate::repo::Repository;
use crate::state::{resolve_head, resolve_head_for_repository, HeadState};
use crate::worktree::{ensure_branch_not_checked_out_in_any_worktree, list_worktrees};

const HEADS_PREFIX: &str = "refs/heads/";

/// Branch create/rename/delete validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BranchError {
    /// `refs/heads/<name>` already exists and `force` was not set.
    #[error("branch '{name}' already exists")]
    AlreadyExists {
        /// Short branch name.
        name: String,
    },
    /// The branch short name failed ref-format checks.
    #[error("invalid branch name: {detail}")]
    InvalidName {
        /// Human-readable detail.
        detail: String,
    },
    /// `refs/heads/<name>` does not exist and no worktree `HEAD` points at it.
    #[error("no branch named '{branch}'")]
    NotFound {
        /// Short branch name.
        branch: String,
    },
}

/// Options for [`rename_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameBranchOptions {
    /// Current short branch name.
    pub old: String,
    /// New short branch name.
    pub new: String,
    /// Replace an existing `refs/heads/<new>` (and its config section) when set.
    pub force: bool,
    /// Validate and preview without updating refs, config, or worktree `HEAD`s.
    pub dry_run: bool,
}

/// Outcome of [`rename_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameBranchOutcome {
    /// Previous short branch name.
    pub old: String,
    /// New short branch name.
    pub new: String,
    /// Tip commit before the rename, when the branch ref existed.
    pub oid: Option<ObjectId>,
    /// Previous tip of `new` when `force` replaced an existing branch.
    pub replaced: Option<ObjectId>,
    /// Whether this outcome was preview-only.
    pub dry_run: bool,
}

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

/// Rename `refs/heads/<old>` to `refs/heads/<new>` through one ref-store transaction.
///
/// Copies the branch reflog (plus a Git-compatible rename entry), renames the
/// `branch.<old>` config section, and retargets every worktree `HEAD` symref that
/// pointed at `old`. When the branch ref is missing but a worktree `HEAD` still
/// points at `old` (unborn branch), only `HEAD` and config are updated.
///
/// # Errors
///
/// - [`BranchError::NotFound`] when `old` is missing and no worktree uses it.
/// - [`BranchError::AlreadyExists`] when `new` exists and `force` is unset.
/// - [`Error::BranchCheckedOutInWorktree`] when `force` would clobber a branch
///   checked out in another worktree.
/// - Ref store, config, and I/O errors are propagated.
pub fn rename_branch(repo: &Repository, opts: &RenameBranchOptions) -> Result<RenameBranchOutcome> {
    if opts.old == opts.new {
        let oid = repo
            .references()
            .resolve_if_exists(&branch_ref(&opts.old))?;
        return Ok(RenameBranchOutcome {
            old: opts.old.clone(),
            new: opts.new.clone(),
            oid,
            replaced: None,
            dry_run: opts.dry_run,
        });
    }

    validate_branch_short_name(&opts.old).map_err(|e| {
        Error::Branch(BranchError::InvalidName {
            detail: e.to_string(),
        })
    })?;
    validate_branch_short_name(&opts.new).map_err(|e| {
        Error::Branch(BranchError::InvalidName {
            detail: e.to_string(),
        })
    })?;

    let old_ref = branch_ref(&opts.old);
    let new_ref = branch_ref(&opts.new);
    let refs = repo.references();
    let old_existing = refs.read_existing(&old_ref)?;
    let new_existing = refs.read_existing(&new_ref)?;
    let head_uses_old = worktree_heads_on_branch(repo, &opts.old)?;

    if old_existing.is_none() && !head_uses_old {
        return Err(Error::Branch(BranchError::NotFound {
            branch: opts.old.clone(),
        }));
    }

    let replaced = if let Some(existing) = &new_existing {
        if !opts.force {
            return Err(Error::Branch(BranchError::AlreadyExists {
                name: opts.new.clone(),
            }));
        }
        ensure_branch_not_checked_out_in_any_worktree(repo, &opts.new)?;
        Some(existing.oid)
    } else {
        None
    };

    let oid = old_existing.as_ref().map(|existing| existing.oid);

    if opts.dry_run {
        return Ok(RenameBranchOutcome {
            old: opts.old.clone(),
            new: opts.new.clone(),
            oid,
            replaced,
            dry_run: true,
        });
    }

    if let Some(existing) = old_existing {
        rename_existing_branch_ref(repo, &old_ref, &new_ref, &existing, new_existing.as_ref())?;
        if let Some(tip) = oid {
            write_rename_reflog(repo, &old_ref, &new_ref, tip)?;
        }
    }

    retarget_branch_head_symrefs(repo, &opts.old, &new_ref)?;
    rename_branch_config_section(repo, &opts.old, &opts.new, opts.force)?;

    Ok(RenameBranchOutcome {
        old: opts.old.clone(),
        new: opts.new.clone(),
        oid,
        replaced,
        dry_run: false,
    })
}

fn branch_ref(short: &str) -> String {
    format!("{HEADS_PREFIX}{short}")
}

fn worktree_heads_on_branch(repo: &Repository, short: &str) -> Result<bool> {
    Ok(list_worktrees(repo)?.into_iter().any(
        |entry| matches!(&entry.head, HeadState::Branch { short_name, .. } if short_name == short),
    ))
}

fn rename_existing_branch_ref(
    repo: &Repository,
    old_ref: &str,
    new_ref: &str,
    old_existing: &crate::refs::handle::ExistingRef,
    new_existing: Option<&crate::refs::handle::ExistingRef>,
) -> Result<()> {
    let mut updates = Vec::new();

    if let Some(new_existing) = new_existing {
        let (expected, no_deref) = expected_for_existing(new_existing);
        updates.push(RefUpdate {
            name: new_ref.to_owned(),
            new_value: Some(RawRef::Direct(old_existing.oid)),
            expected,
            reflog: None,
            flags: RefUpdateFlags {
                no_deref,
                ..RefUpdateFlags::default()
            },
        });
    } else {
        updates.push(RefUpdate {
            name: new_ref.to_owned(),
            new_value: Some(RawRef::Direct(old_existing.oid)),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        });
    }

    let (expected, no_deref) = expected_for_existing(old_existing);
    updates.push(RefUpdate {
        name: old_ref.to_owned(),
        new_value: None,
        expected,
        reflog: None,
        flags: RefUpdateFlags {
            no_deref,
            ..RefUpdateFlags::default()
        },
    });

    refs::commit_ref_updates_on_store(repo.refs(), &updates)?;
    Ok(())
}

fn expected_for_existing(existing: &crate::refs::handle::ExistingRef) -> (Expected, bool) {
    match &existing.raw {
        RawRef::Symbolic(target) => (Expected::Symref(target.clone()), true),
        RawRef::Direct(oid) => (Expected::Oid(*oid), false),
    }
}

fn write_rename_reflog(
    repo: &Repository,
    old_ref: &str,
    new_ref: &str,
    tip: ObjectId,
) -> Result<()> {
    let store = repo.refs();
    let mut entries = Vec::new();
    store.for_each_reflog_entry(old_ref, false, &mut |entry| {
        entries.push(entry.clone());
        ControlFlow::Continue(())
    })?;
    store.replace_reflog(new_ref, entries)?;
    let config =
        ConfigSet::load(repo.environment(), Some(repo.git_dir()), true).unwrap_or_default();
    let identity = checkout_reflog_identity(repo);
    let message = format!("Branch: renamed {old_ref} to {new_ref}");
    refs::append_reflog_with_config(
        repo.git_dir(),
        new_ref,
        &tip,
        &tip,
        &identity,
        &message,
        false,
        Some(&config),
    )?;
    store.delete_reflog(old_ref)?;
    Ok(())
}

fn retarget_branch_head_symrefs(repo: &Repository, old_short: &str, new_ref: &str) -> Result<()> {
    for entry in list_worktrees(repo)? {
        if matches!(&entry.head, HeadState::Branch { short_name, .. } if short_name == old_short) {
            fs::write(entry.admin_dir.join("HEAD"), format!("ref: {new_ref}\n"))
                .map_err(Error::Io)?;
        }
    }
    Ok(())
}

fn rename_branch_config_section(
    repo: &Repository,
    old_short: &str,
    new_short: &str,
    force: bool,
) -> Result<()> {
    let path = crate::repo::local_config_write_path(repo.git_dir());
    let content = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    if content.trim().is_empty() {
        return Ok(());
    }
    let mut file = ConfigFile::parse(&path, &content, ConfigScope::Local)?;
    let old_section = format!("branch.{old_short}");
    let new_section = format!("branch.{new_short}");
    let mut changed = false;
    if force {
        changed |= file.remove_section(&new_section)?;
    }
    changed |= file.rename_section(&old_section, &new_section)?;
    if changed {
        file.write()?;
        repo.reload_config()?;
    }
    Ok(())
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
            dry_run: false,
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
