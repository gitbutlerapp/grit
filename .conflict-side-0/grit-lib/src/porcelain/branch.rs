//! Local branches: create one without switching `HEAD`, list them, delete one, rename one, and configure upstream.

use std::fs;

use crate::branch_tracking::{
    remote_tracking_ref_is_mapped, upstream_branch, upstream_config_for_branch_ref,
};
use crate::check_ref_format::validate_branch_short_name;
use crate::config::{parse_git_config_maybe_bool, ConfigFile, ConfigScope, ConfigSet};
use crate::error::{Error, Result};
use crate::merge_base::is_ancestor;
use crate::objects::ObjectId;
use crate::porcelain::checkout_reflog::{
    branch_create_reflog_source_label, checkout_reflog_identity, create_branch_ref_with_reflog,
    reset_branch_ref_with_reflog,
};
use crate::ref_storage::RefStorageFormat;
use crate::refs;
use crate::refs::store::{
    Expected, RawRef, RefCaseOnlyRename, RefReflogRename, RefUpdate, RefUpdateFlags, ReflogUpdate,
};
use crate::repo::Repository;
use crate::rev_parse::peel_to_commit_for_merge_base;
use crate::state::{resolve_head, resolve_head_for_repository, HeadState};
use crate::worktree::{
    admin_dirs_match, ensure_branch_not_checked_out_in_any_worktree, list_worktrees,
    worktree_admin_dir,
};

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

    if existing.is_some() && !opts.force {
        return Err(Error::Branch(BranchError::AlreadyExists {
            name: name.clone(),
        }));
    }
    if existing.is_some() {
        ensure_branch_checked_out(repo, name)?;
    }

    let tracking = resolve_create_tracking(repo, name, &source_label, start_oid, opts.track)?;

    if opts.dry_run {
        return Ok(CreateBranchOutcome {
            name: name.clone(),
            oid: start_oid,
        });
    }

    if let Some(existing_ref) = &existing {
        reset_branch_ref_with_reflog(repo, &branch_ref, start_oid, &source_label, existing_ref)?;
    } else {
        create_branch_ref_with_reflog(repo, &branch_ref, start_oid, &source_label)?;
    }

    if let Some((remote, merge)) = tracking {
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
    let allow_local = track == TrackMode::Always;
    let Some(tracking_ref) = resolve_start_label_as_upstream_ref(repo, start_label, allow_local)?
    else {
        if track == TrackMode::Always {
            return Err(Error::Branch(BranchError::UpstreamNotTracking {
                upstream: start_label.to_owned(),
            }));
        }
        return Ok(None);
    };
    let is_remote = tracking_ref.starts_with("refs/remotes/");
    if is_remote && !remote_tracking_ref_is_mapped(repo, &tracking_ref)? {
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
    validate_branch_short_name(&opts.old).map_err(|e| {
        Error::Branch(BranchError::InvalidName {
            name: opts.old.clone(),
            reason: e.to_string(),
        })
    })?;
    validate_branch_short_name(&opts.new).map_err(|e| {
        Error::Branch(BranchError::InvalidName {
            name: opts.new.clone(),
            reason: e.to_string(),
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
            name: opts.old.clone(),
        }));
    }

    if opts.old == opts.new {
        return Ok(RenameBranchOutcome {
            old: opts.old.clone(),
            new: opts.new.clone(),
            oid: old_existing.as_ref().map(|existing| existing.oid),
            replaced: None,
            dry_run: opts.dry_run,
        });
    }

    let case_only_same_branch = old_existing.as_ref().is_some_and(|old| {
        opts.old != opts.new
            && opts.old.eq_ignore_ascii_case(&opts.new)
            && new_existing
                .as_ref()
                .is_none_or(|newer| newer.oid == old.oid)
    });

    let replaced = if let Some(existing) = &new_existing {
        if case_only_same_branch {
            None
        } else if !opts.force {
            return Err(Error::Branch(BranchError::AlreadyExists {
                name: opts.new.clone(),
            }));
        } else {
            ensure_branch_not_checked_out_in_any_worktree(repo, &opts.new)?;
            Some(existing.oid)
        }
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
        let reflog_rename = branch_rename_reflog_update(repo, &old_ref, &new_ref, existing.oid);
        if case_only_same_branch
            && RefStorageFormat::detect(repo.git_dir())? == RefStorageFormat::Files
        {
            refs::commit_ref_batch_on_store(
                repo.refs(),
                &[],
                &[reflog_rename],
                &[RefCaseOnlyRename {
                    from: old_ref.clone(),
                    to: new_ref.clone(),
                }],
            )?;
        } else {
            rename_existing_branch_ref(
                repo,
                &old_ref,
                &new_ref,
                &existing,
                new_existing.as_ref(),
                reflog_rename,
            )?;
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

fn branch_rename_reflog_update(
    repo: &Repository,
    old_ref: &str,
    new_ref: &str,
    tip: ObjectId,
) -> RefReflogRename {
    RefReflogRename {
        from: old_ref.to_owned(),
        to: new_ref.to_owned(),
        append: ReflogUpdate {
            identity: checkout_reflog_identity(repo),
            message: format!("Branch: renamed {old_ref} to {new_ref}"),
            time: repo.identity_now(),
            old_oid: Some(tip),
            new_oid: Some(tip),
        },
    }
}

fn rename_existing_branch_ref(
    repo: &Repository,
    old_ref: &str,
    new_ref: &str,
    old_existing: &crate::refs::handle::ExistingRef,
    new_existing: Option<&crate::refs::handle::ExistingRef>,
    reflog_rename: RefReflogRename,
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

    refs::commit_ref_updates_on_store(repo.refs(), &updates, &[reflog_rename])?;
    Ok(())
}

fn expected_for_existing(existing: &crate::refs::handle::ExistingRef) -> (Expected, bool) {
    match &existing.raw {
        RawRef::Symbolic(target) => (Expected::Symref(target.clone()), true),
        RawRef::Direct(oid) => (Expected::Oid(*oid), false),
    }
}

fn retarget_branch_head_symrefs(repo: &Repository, old_short: &str, new_ref: &str) -> Result<()> {
    let old_ref = branch_ref(old_short);
    let current_admin = worktree_admin_dir(repo);
    let head_update = RefUpdate {
        name: "HEAD".to_owned(),
        new_value: Some(RawRef::Symbolic(new_ref.to_owned())),
        expected: Expected::Symref(old_ref.clone()),
        reflog: None,
        flags: RefUpdateFlags {
            no_deref: true,
            ..RefUpdateFlags::default()
        },
    };
    for entry in list_worktrees(repo)? {
        if !matches!(&entry.head, HeadState::Branch { short_name, .. } if short_name == old_short) {
            continue;
        }
        if admin_dirs_match(&entry.admin_dir, &current_admin) {
            continue;
        }
        let store = repo
            .caches()
            .open_ref_store(&entry.admin_dir, repo.environment())?;
        refs::commit_ref_updates_on_store(store.as_ref(), std::slice::from_ref(&head_update), &[])?;
    }
    if repository_head_points_at(repo, &old_ref)? {
        refs::commit_ref_updates_on_store(repo.refs(), &[head_update], &[])?;
    }
    Ok(())
}

fn repository_head_points_at(repo: &Repository, refname: &str) -> Result<bool> {
    Ok(matches!(
        repo.refs().read_raw("HEAD")?,
        Some(RawRef::Symbolic(target)) if target == refname
    ))
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

/// Display name for configured upstream (`origin/main` from remote + merge ref).
#[must_use]
pub fn upstream_tracking_display(remote: &str, merge: &str) -> String {
    if remote == "." {
        return merge
            .strip_prefix("refs/heads/")
            .unwrap_or(merge)
            .to_owned();
    }
    merge
        .strip_prefix("refs/heads/")
        .map(|tail| format!("{remote}/{tail}"))
        .unwrap_or_else(|| merge.to_owned())
}

fn resolve_set_upstream(
    repo: &Repository,
    branch: &str,
    upstream: &str,
) -> Result<(String, String, String)> {
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
    let display = upstream_tracking_display(&remote, &merge);
    Ok((remote, merge, display))
}

/// Validate [`set_upstream`] inputs and return the canonical upstream display name.
///
/// Performs the same checks as [`set_upstream`] without writing config.
pub fn validate_set_upstream(repo: &Repository, branch: &str, upstream: &str) -> Result<String> {
    resolve_set_upstream(repo, branch, upstream).map(|(_, _, display)| display)
}

/// Configure `branch.<branch>.remote` and `branch.<branch>.merge` from a resolved upstream ref.
pub fn set_upstream(repo: &Repository, branch: &str, upstream: &str) -> Result<()> {
    let (remote, merge, _) = resolve_set_upstream(repo, branch, upstream)?;
    write_branch_upstream_config(repo, branch, &remote, &merge)
}

fn validate_unset_upstream(repo: &Repository, branch: &str) -> Result<()> {
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
    let config = repo.config()?;
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    if config.get(&remote_key).is_none() && config.get(&merge_key).is_none() {
        return Err(Error::Branch(BranchError::NoUpstream {
            name: branch.to_owned(),
        }));
    }
    Ok(())
}

/// Validate [`unset_upstream`] inputs without clearing config.
pub fn preview_unset_upstream(repo: &Repository, branch: &str) -> Result<()> {
    validate_unset_upstream(repo, branch)
}

/// Clear upstream configuration for `branch`.
pub fn unset_upstream(repo: &Repository, branch: &str) -> Result<()> {
    validate_unset_upstream(repo, branch)?;
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
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
    let (oid, label) = match start {
        Some(point) => (point.oid, point.label.clone()),
        None => {
            let head = resolve_head(repo.git_dir())?;
            let oid = head
                .oid()
                .copied()
                .ok_or(Error::Branch(BranchError::NoCommits))?;
            let label = branch_create_reflog_source_label(&head);
            (oid, label)
        }
    };
    let commit = validate_start_commit(repo, oid, &label)?;
    Ok((commit, label))
}

fn validate_start_commit(repo: &Repository, oid: ObjectId, label: &str) -> Result<ObjectId> {
    peel_to_commit_for_merge_base(repo, oid).map_err(|err| match err {
        Error::ObjectNotFound(_) => Error::Branch(BranchError::InvalidStartPoint {
            label: label.to_owned(),
            reason: "object not found".into(),
        }),
        Error::InvalidRef(msg) | Error::CorruptObject(msg) => {
            Error::Branch(BranchError::InvalidStartPoint {
                label: label.to_owned(),
                reason: msg,
            })
        }
        other => other,
    })
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
        None | Some("true") | Some("default") => true,
        Some("inherit") | Some("always") => true,
        Some("false") => false,
        Some("simple") => tracking_ref
            .strip_prefix("refs/remotes/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, tail)| tail == branch_short),
        Some(value) => parse_git_config_maybe_bool(value).unwrap_or(true),
    }
}

/// Resolve a start reflog label to an upstream ref for tracking configuration.
///
/// Bare local branch names (for example the current branch from `HEAD`) are not
/// treated as `origin/<name>`; only explicit `remote/branch` or full ref forms qualify.
fn resolve_start_label_as_upstream_ref(
    repo: &Repository,
    label: &str,
    allow_local: bool,
) -> Result<Option<String>> {
    if label.starts_with("refs/remotes/") {
        return Ok(repo
            .references()
            .resolve_if_exists(label)?
            .map(|_| label.to_owned()));
    }
    if allow_local {
        if label.starts_with("refs/heads/") {
            return Ok(repo
                .references()
                .resolve_if_exists(label)?
                .map(|_| label.to_owned()));
        }
        if !label.contains('/') && !label.starts_with("refs/") {
            let local = format!("{HEADS_PREFIX}{label}");
            if repo.references().resolve_if_exists(&local)?.is_some() {
                return Ok(Some(local));
            }
        }
    }
    if label.contains('/') && !label.starts_with("refs/") {
        let remote = format!("refs/remotes/{label}");
        if repo.references().resolve_if_exists(&remote)?.is_some() {
            return Ok(Some(remote));
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
    use crate::porcelain::commit::{create_commit, CommitRequest, HookPolicy};
    use crate::progress::NullProgress;
    use crate::refs;
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
            ..Default::default()
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
    fn upstream_tracking_display_local_branch_uses_short_name() {
        assert_eq!(upstream_tracking_display(".", "refs/heads/base"), "base");
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
