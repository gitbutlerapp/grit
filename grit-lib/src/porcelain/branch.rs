//! Local branches: create one without switching `HEAD`, list them, delete one, and configure upstream.

use crate::branch_tracking::{
    remote_tracking_ref_is_mapped, upstream_branch, upstream_config_for_branch_ref,
};
use crate::check_ref_format::validate_branch_short_name;
use crate::config::{parse_git_config_maybe_bool, ConfigSet};
use crate::error::{Error, Result};
use crate::merge_base::is_ancestor;
use crate::objects::ObjectId;
use crate::porcelain::checkout_reflog::{
    branch_create_reflog_source_label, create_branch_ref_with_reflog, reset_branch_ref_with_reflog,
};
use crate::repo::Repository;
use crate::state::{resolve_head, resolve_head_for_repository, HeadState};
use crate::worktree::ensure_branch_not_checked_out_in_any_worktree;

pub use crate::porcelain::branch_error::{BranchError, DeleteMode, StartPoint, TrackMode};

const HEADS_PREFIX: &str = "refs/heads/";

/// Inputs for [`create_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBranchOptions {
    /// Short branch name (without `refs/heads/`).
    pub name: String,
    /// Start commit and reflog label; `None` uses the current `HEAD` commit.
    pub start: Option<StartPoint>,
    /// Overwrite an existing branch ref (refuses when checked out in a worktree).
    pub force: bool,
    /// Whether to write `branch.<name>.remote` / `.merge` after creating the ref.
    pub track: TrackMode,
    /// Validate only; leave refs, config, and reflogs unchanged.
    pub dry_run: bool,
}

impl CreateBranchOptions {
    /// Convenience for `grit branch <name>` (start at `HEAD`, no force, auto track, live).
    #[must_use]
    pub fn at_head(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            start: None,
            force: false,
            track: TrackMode::Auto,
            dry_run: false,
        }
    }
}

/// Outcome of [`create_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateBranchOutcome {
    /// Short branch name.
    pub name: String,
    /// Tip commit the branch points at (or would point at when `dry_run`).
    pub oid: ObjectId,
}

/// Create or reset `refs/heads/<name>` without checking it out.
///
/// # Errors
///
/// Returns [`Error::Branch`] for invalid names, existing branches, missing commits, checked-out
/// conflicts, and upstream tracking failures.
pub fn create_branch(repo: &Repository, opts: &CreateBranchOptions) -> Result<CreateBranchOutcome> {
    let name = &opts.name;
    validate_branch_short_name(name).map_err(|reason| {
        Error::Branch(BranchError::InvalidName {
            name: name.clone(),
            reason: reason.to_string(),
        })
    })?;

    let (start_oid, source_label) = resolve_create_start(repo, opts.start.as_ref())?;
    let branch_ref = format!("{HEADS_PREFIX}{name}");
    let existing = repo.references().read_existing(&branch_ref)?;

    if let Some(existing_ref) = &existing {
        if !opts.force {
            return Err(Error::Branch(BranchError::AlreadyExists {
                name: name.clone(),
            }));
        }
        ensure_branch_checked_out(repo, name)?;
        if opts.dry_run {
            return Ok(CreateBranchOutcome {
                name: name.clone(),
                oid: start_oid,
            });
        }
        reset_branch_ref_with_reflog(repo, &branch_ref, start_oid, &source_label, existing_ref)?;
    } else {
        if opts.dry_run {
            return Ok(CreateBranchOutcome {
                name: name.clone(),
                oid: start_oid,
            });
        }
        create_branch_ref_with_reflog(repo, &branch_ref, start_oid, &source_label)?;
    }

    if let Some((remote, merge)) =
        resolve_create_tracking(repo, name, &source_label, start_oid, opts.track)?
    {
        write_branch_upstream_config(repo, name, &remote, &merge)?;
    }

    Ok(CreateBranchOutcome {
        name: name.clone(),
        oid: start_oid,
    })
}

/// Resolve upstream `(remote, merge)` for branch creation, shared with [`crate::porcelain::switch`].
///
/// # Errors
///
/// Returns [`Error::Branch`] when `track` is [`TrackMode::Always`] but no upstream can be configured.
pub fn resolve_create_tracking(
    repo: &Repository,
    branch_short: &str,
    start_label: &str,
    _start_oid: ObjectId,
    track: TrackMode,
) -> Result<Option<(String, String)>> {
    if track == TrackMode::Never {
        return Ok(None);
    }
    let Some(tracking_ref) = resolve_label_as_upstream_ref(repo, start_label)? else {
        if track == TrackMode::Always {
            return Err(Error::Branch(BranchError::UpstreamNotTracking {
                upstream: start_label.to_owned(),
            }));
        }
        return Ok(None);
    };
    if !remote_tracking_ref_is_mapped(repo, &tracking_ref)? {
        if track == TrackMode::Always {
            return Err(Error::Branch(BranchError::UpstreamNotTracking {
                upstream: start_label.to_owned(),
            }));
        }
        return Ok(None);
    }
    let config = repo.config()?;
    if track == TrackMode::Auto
        && !branch_auto_setup_merge_allows(&config, branch_short, &tracking_ref)
    {
        return Ok(None);
    }
    upstream_config_for_branch_ref(&tracking_ref)
        .ok_or_else(|| {
            Error::Branch(BranchError::UpstreamNotTracking {
                upstream: start_label.to_owned(),
            })
        })
        .map(Some)
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
    pub current: Option<String>,
    /// Every local branch, sorted by name.
    pub branches: Vec<LocalBranch>,
}

/// List the local branches (`refs/heads/*`) of `repo`, sorted by name.
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

/// Delete `refs/heads/<name>`, its reflog, and the `branch.<name>` config section.
pub fn delete_branch(
    repo: &Repository,
    name: &str,
    mode: DeleteMode,
    dry_run: bool,
) -> Result<DeletedBranch> {
    validate_branch_short_name(name).map_err(|reason| {
        Error::Branch(BranchError::InvalidName {
            name: name.to_owned(),
            reason: reason.to_string(),
        })
    })?;

    let head = resolve_head_for_repository(repo)?;
    if matches!(&head, HeadState::Branch { short_name, .. } if short_name == name) {
        return Err(checked_out_error(repo, name)?);
    }
    ensure_branch_checked_out(repo, name)?;

    let branch_ref = format!("{HEADS_PREFIX}{name}");
    let existing = repo
        .references()
        .read_existing(&branch_ref)?
        .ok_or_else(|| {
            Error::Branch(BranchError::NotFound {
                name: name.to_owned(),
            })
        })?;
    let oid = existing.oid;

    if mode == DeleteMode::Safe {
        let (merged, base_label) = merge_status(repo, name, oid, &head)?;
        if !merged {
            return Err(Error::Branch(BranchError::NotFullyMerged {
                name: name.to_owned(),
                base: base_label,
            }));
        }
    }

    if dry_run {
        return Ok(DeletedBranch {
            name: name.to_owned(),
            oid,
        });
    }

    repo.references().delete_verified(&branch_ref, &existing)?;
    let section = branch_config_section(name);
    repo.config_handle().remove_local_section(&section)?;
    Ok(DeletedBranch {
        name: name.to_owned(),
        oid,
    })
}

/// Configure `branch.<branch>.remote` and `branch.<branch>.merge` from a resolved upstream ref.
pub fn set_upstream(repo: &Repository, branch: &str, upstream: &str) -> Result<()> {
    validate_branch_short_name(branch).map_err(|reason| {
        Error::Branch(BranchError::InvalidName {
            name: branch.to_owned(),
            reason: reason.to_string(),
        })
    })?;
    let branch_ref = format!("{HEADS_PREFIX}{branch}");
    if repo.references().read_existing(&branch_ref)?.is_none() {
        return Err(Error::Branch(BranchError::NotFound {
            name: branch.to_owned(),
        }));
    }
    let full = resolve_upstream_ref(repo, upstream).ok_or_else(|| {
        Error::Branch(BranchError::UpstreamNotTracking {
            upstream: upstream.to_owned(),
        })
    })?;
    let (remote, merge) = upstream_config_for_branch_ref(&full).ok_or_else(|| {
        Error::Branch(BranchError::UpstreamNotTracking {
            upstream: upstream.to_owned(),
        })
    })?;
    write_branch_upstream_config(repo, branch, &remote, &merge)
}

/// Clear upstream configuration for `branch`.
pub fn unset_upstream(repo: &Repository, branch: &str) -> Result<()> {
    validate_branch_short_name(branch).map_err(|reason| {
        Error::Branch(BranchError::InvalidName {
            name: branch.to_owned(),
            reason: reason.to_string(),
        })
    })?;
    let config = repo.config()?;
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    if config.get(&remote_key).is_none() && config.get(&merge_key).is_none() {
        return Err(Error::Branch(BranchError::NoUpstream {
            name: branch.to_owned(),
        }));
    }
    let handle = repo.config_handle();
    handle.unset_local(&remote_key)?;
    handle.unset_local(&merge_key)?;
    Ok(())
}

pub(crate) fn write_branch_upstream_config(
    repo: &Repository,
    branch_short: &str,
    remote: &str,
    merge: &str,
) -> Result<()> {
    let handle = repo.config_handle();
    handle.set_local(&format!("branch.{branch_short}.remote"), remote)?;
    handle.set_local(&format!("branch.{branch_short}.merge"), merge)?;
    Ok(())
}

fn branch_config_section(branch_short: &str) -> String {
    format!("branch.{branch_short}")
}

fn resolve_create_start(
    repo: &Repository,
    start: Option<&StartPoint>,
) -> Result<(ObjectId, String)> {
    match start {
        Some(point) => Ok((point.oid, point.label.clone())),
        None => {
            let head = resolve_head(repo.git_dir())?;
            let oid = head
                .oid()
                .copied()
                .ok_or(Error::Branch(BranchError::NoCommits))?;
            let label = branch_create_reflog_source_label(&head);
            Ok((oid, label))
        }
    }
}

fn merge_status(
    repo: &Repository,
    name: &str,
    tip: ObjectId,
    head: &HeadState,
) -> Result<(bool, String)> {
    if let Some(upstream) = upstream_branch(repo, name) {
        let merged = is_ancestor(repo, tip, upstream.oid)?;
        return Ok((merged, upstream.name));
    }
    let base_oid = head.oid().copied();
    let base_label = match head {
        HeadState::Branch { short_name, .. } => short_name.clone(),
        _ => "HEAD".to_owned(),
    };
    match base_oid {
        Some(base) => Ok((is_ancestor(repo, tip, base)?, base_label)),
        None => Ok((false, base_label)),
    }
}

fn ensure_branch_checked_out(repo: &Repository, name: &str) -> Result<()> {
    if let Err(err) = ensure_branch_not_checked_out_in_any_worktree(repo, name) {
        return Err(map_checked_out_err(err, name));
    }
    Ok(())
}

fn checked_out_error(repo: &Repository, name: &str) -> Result<Error> {
    let path = repo
        .work_tree()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| repo.git_dir().display().to_string());
    Err(Error::Branch(BranchError::CheckedOut {
        name: name.to_owned(),
        worktree: path,
    }))
}

fn map_checked_out_err(err: Error, name: &str) -> Error {
    match err {
        Error::BranchCheckedOutInWorktree { worktree_path, .. } => {
            Error::Branch(BranchError::CheckedOut {
                name: name.to_owned(),
                worktree: worktree_path,
            })
        }
        other => other,
    }
}

fn branch_auto_setup_merge_allows(
    config: &ConfigSet,
    branch_short: &str,
    tracking_ref: &str,
) -> bool {
    let auto_setup = config.get("branch.autoSetupMerge");
    let raw = auto_setup.as_deref();
    match raw {
        None | Some("true") | Some("default") | Some("inherit") | Some("always") => true,
        Some("false") => false,
        Some("simple") => tracking_ref
            .strip_prefix("refs/remotes/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, tail)| tail == branch_short),
        Some(value) => parse_git_config_maybe_bool(value).unwrap_or(true),
    }
}

fn resolve_label_as_upstream_ref(repo: &Repository, label: &str) -> Result<Option<String>> {
    if label.starts_with("refs/remotes/") {
        return Ok(repo
            .references()
            .resolve_if_exists(label)?
            .map(|_| label.to_owned()));
    }
    if label.starts_with("refs/heads/") {
        return Ok(Some(label.to_owned()));
    }
    for candidate in [
        format!("refs/remotes/{label}"),
        format!("refs/remotes/origin/{label}"),
    ] {
        if repo.references().resolve_if_exists(&candidate)?.is_some() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn resolve_upstream_ref(repo: &Repository, upstream: &str) -> Option<String> {
    if upstream.starts_with("refs/remotes/") || upstream.starts_with("refs/heads/") {
        return repo
            .references()
            .resolve_if_exists(upstream)
            .ok()
            .flatten()
            .map(|_| upstream.to_owned());
    }
    [
        format!("refs/remotes/{upstream}"),
        format!("refs/heads/{upstream}"),
    ]
    .into_iter()
    .find(|candidate| {
        repo.references()
            .resolve_if_exists(candidate)
            .ok()
            .flatten()
            .is_some()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::refs;
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
        create_branch(&repo, &CreateBranchOptions::at_head("zeta")).unwrap();
        create_branch(&repo, &CreateBranchOptions::at_head("alpha")).unwrap();
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
        let err = delete_branch(&repo, &current, DeleteMode::Force, false).unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Branch(BranchError::CheckedOut { name, .. }) if name == &current
            ),
            "{err:?}"
        );
    }

    #[test]
    fn delete_missing_branch_is_typed() {
        let (_tmp, repo) = init();
        commit(&repo, "one");
        let err = delete_branch(&repo, "nope", DeleteMode::Force, false).unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Branch(BranchError::NotFound { name }) if name == "nope"
            ),
            "{err:?}"
        );
    }

    #[test]
    fn delete_merged_branch_returns_old_tip_and_removes_ref() {
        let (_tmp, repo) = init();
        let tip = commit(&repo, "one");
        create_branch(&repo, &CreateBranchOptions::at_head("topic")).unwrap();
        commit(&repo, "two");

        let deleted = delete_branch(&repo, "topic", DeleteMode::Safe, false).unwrap();
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
        create_branch(&repo, &CreateBranchOptions::at_head("topic")).unwrap();
        let current = current(&repo);
        refs::write_ref(repo.git_dir(), &format!("{HEADS_PREFIX}{current}"), &first).unwrap();

        let err = delete_branch(&repo, "topic", DeleteMode::Safe, false).unwrap_err();
        assert!(
            matches!(
                &err,
                Error::Branch(BranchError::NotFullyMerged { name, .. }) if name == "topic"
            ),
            "{err:?}"
        );
        assert!(branch_exists(&repo, "topic"));

        let deleted = delete_branch(&repo, "topic", DeleteMode::Force, false).unwrap();
        assert_eq!(deleted.oid, second);
        assert!(!branch_exists(&repo, "topic"));
    }
}
