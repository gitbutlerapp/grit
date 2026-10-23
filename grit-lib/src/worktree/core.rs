//! Linked worktree registry: discovery, listing, and admin-dir layout.
//!
//! Git stores linked worktrees under `<common-git-dir>/worktrees/<id>/` with
//! `gitdir`, `commondir`, `HEAD`, and optional `locked` / `prunable` files.
//! This module reads that layout and implements linked worktree add/remove.

use std::fs;
use std::path::{Path, PathBuf};

use crate::branch_tracking::write_branch_upstream_config;
use crate::check_ref_format::validate_branch_short_name;
use crate::config::ConfigSet;
#[cfg(test)]
use crate::environment::RepositoryOptions;
use crate::error::{Error, Result};
use crate::git_path::{absolute_path_for_gitfile_stored_path, path_for_disk_compare};
use crate::objects::{parse_commit, ObjectKind};
use crate::porcelain::branch::{resolve_create_tracking, TrackMode};
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::checkout_reflog::{
    create_branch_ref_with_reflog, reset_branch_ref_with_reflog,
};
use crate::porcelain::status::{status, StatusOptions};
use crate::progress::NullProgress;
use crate::refs;
use crate::repo::{common_git_dir_for_config, Repository};
use crate::rev_parse::resolve_revision;
use crate::state::{resolve_head, HeadState};

/// One row returned by [`list_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Absolute path to the working tree (for bare main worktree, the common git dir).
    pub path: PathBuf,
    /// Resolved HEAD for this worktree.
    pub head: HeadState,
    /// Whether this worktree is bare (only the main entry can be bare).
    pub is_bare: bool,
    /// True when `<admin>/locked` exists.
    pub is_locked: bool,
    /// Contents of the `locked` file when non-empty.
    pub lock_reason: Option<String>,
    /// Administrative directory: common git dir for main, else `worktrees/<id>/`.
    pub admin_dir: PathBuf,
    /// True when the working tree directory is gone (Git `prunable` in `worktree list`).
    pub is_prunable: bool,
}

/// Shared git directory for `git_dir` (follows `commondir` for linked worktrees).
#[must_use]
pub fn common_git_dir(git_dir: &Path) -> PathBuf {
    common_git_dir_for_config(git_dir)
}

/// Resolve HEAD for a linked worktree admin dir (`HEAD` local, branch refs in `common`).
#[must_use]
pub fn resolve_linked_head(admin: &Path, _common: &Path) -> HeadState {
    resolve_head(admin).unwrap_or(HeadState::Invalid)
}

/// Number of registered worktrees (main + linked entries under `worktrees/`).
#[must_use]
pub fn registered_worktree_count(common: &Path) -> usize {
    let worktrees_dir = common.join("worktrees");
    if !worktrees_dir.is_dir() {
        return 1;
    }
    let linked = fs::read_dir(&worktrees_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .count();
    1 + linked
}

/// Whether `common` is configured as a bare repository (`core.bare=true`).
#[must_use]
pub fn is_bare_repository(common: &Path) -> bool {
    ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(common),
        true,
    )
    .ok()
    .and_then(|cfg| cfg.get_bool("core.bare"))
    .and_then(|r| r.ok())
    .unwrap_or_else(|| {
        // Heuristic when config is missing: bare repos usually are not named `.git`.
        !common.ends_with(".git") && common.join("config").is_file()
    })
}

/// Where a local branch is checked out among registered worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchWorktreeCheckout {
    /// Short branch name (e.g. `feat`).
    pub branch: String,
    /// Working tree path for the checkout.
    pub worktree_path: PathBuf,
    /// Administrative git directory for that worktree.
    pub admin_dir: PathBuf,
}

/// Administrative git directory for the worktree `repo` was opened from.
#[must_use]
pub fn worktree_admin_dir(repo: &Repository) -> PathBuf {
    let common = common_git_dir(repo.git_dir());
    if repo.git_dir().starts_with(common.join("worktrees")) {
        repo.git_dir().to_path_buf()
    } else {
        common
    }
}

fn head_is_branch(head: &HeadState, branch_short_name: &str) -> bool {
    match head {
        HeadState::Branch {
            short_name,
            refname,
            ..
        } => {
            short_name == branch_short_name || refname == &format!("refs/heads/{branch_short_name}")
        }
        _ => false,
    }
}

pub(crate) fn admin_dirs_match(a: &Path, b: &Path) -> bool {
    path_for_disk_compare(a) == path_for_disk_compare(b) || a == b
}

/// Find a worktree that has `branch_short_name` checked out, optionally skipping one admin dir.
///
/// `exclude_admin_dir` is typically the current worktree's admin dir so a switch to the branch
/// already checked out here is allowed.
pub fn find_worktree_with_branch_checked_out(
    repo: &Repository,
    branch_short_name: &str,
    exclude_admin_dir: Option<&Path>,
) -> Result<Option<BranchWorktreeCheckout>> {
    for entry in list_worktrees(repo)? {
        if exclude_admin_dir.is_some_and(|ex| admin_dirs_match(&entry.admin_dir, ex)) {
            continue;
        }
        if head_is_branch(&entry.head, branch_short_name) {
            return Ok(Some(BranchWorktreeCheckout {
                branch: branch_short_name.to_owned(),
                worktree_path: entry.path.clone(),
                admin_dir: entry.admin_dir.clone(),
            }));
        }
    }
    Ok(None)
}

/// Refuse when another worktree already has `branch_short_name` checked out.
///
/// # Errors
///
/// Returns [`Error::BranchCheckedOutInWorktree`] when a different worktree holds the branch.
pub fn ensure_branch_not_checked_out_elsewhere(
    repo: &Repository,
    branch_short_name: &str,
) -> Result<()> {
    let exclude = worktree_admin_dir(repo);
    if let Some(found) =
        find_worktree_with_branch_checked_out(repo, branch_short_name, Some(&exclude))?
    {
        return Err(branch_checked_out_error(
            branch_short_name,
            &found.worktree_path,
        ));
    }
    Ok(())
}

/// Refuse when any worktree (including linked) has `branch_short_name` checked out.
///
/// # Errors
///
/// Returns [`Error::BranchCheckedOutInWorktree`] when any registered worktree holds the branch.
pub fn ensure_branch_not_checked_out_in_any_worktree(
    repo: &Repository,
    branch_short_name: &str,
) -> Result<()> {
    if let Some(found) = find_worktree_with_branch_checked_out(repo, branch_short_name, None)? {
        return Err(branch_checked_out_error(
            branch_short_name,
            &found.worktree_path,
        ));
    }
    Ok(())
}

fn branch_checked_out_error(branch: &str, worktree_path: &Path) -> Error {
    Error::BranchCheckedOutInWorktree {
        branch: branch.to_owned(),
        worktree_path: worktree_path.display().to_string(),
    }
}

/// Enumerate the main and linked worktrees for `repo`.
///
/// Order matches Git: main worktree first, then linked worktrees sorted by admin id.
pub fn list_worktrees(repo: &Repository) -> Result<Vec<WorktreeEntry>> {
    let common = common_git_dir(repo.git_dir());
    let mut entries = Vec::new();

    let bare = is_bare_repository(&common);
    let main_path = if bare {
        path_for_disk_compare(&common)
    } else if let Some(wt) = repo.work_tree() {
        // When opened from the main worktree, use the discovered work tree path.
        if repo.git_dir() == common || !repo.git_dir().starts_with(common.join("worktrees")) {
            path_for_disk_compare(wt)
        } else {
            path_for_disk_compare(common.parent().unwrap_or(&common))
        }
    } else {
        path_for_disk_compare(common.parent().unwrap_or(&common))
    };

    let main_head = resolve_head(&common).unwrap_or(HeadState::Invalid);
    entries.push(WorktreeEntry {
        path: main_path,
        head: main_head,
        is_bare: bare,
        is_locked: false,
        lock_reason: None,
        admin_dir: common.clone(),
        is_prunable: false,
    });

    let worktrees_dir = common.join("worktrees");
    if !worktrees_dir.is_dir() {
        return Ok(entries);
    }

    let mut names: Vec<String> = fs::read_dir(&worktrees_dir)
        .map_err(Error::Io)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    for name in names {
        let admin = worktrees_dir.join(&name);
        let wt_head = resolve_linked_head(&admin, &common);
        let wt_path = read_worktree_path(&admin)?;
        let (is_locked, lock_reason) = read_lock_state(&admin)?;
        let is_prunable = worktree_entry_is_prunable(&admin, &wt_path, is_locked);
        entries.push(WorktreeEntry {
            path: wt_path,
            head: wt_head,
            is_bare: false,
            is_locked,
            lock_reason,
            admin_dir: admin,
            is_prunable,
        });
    }

    Ok(entries)
}

/// Last path component of `path`, without trailing directory separators (Git `worktree_basename`).
#[must_use]
pub fn worktree_path_basename(path: &Path) -> String {
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_owned()
}

/// Sanitize a path basename for use as `worktrees/<id>/` (Git `sanitize_refname_component`).
#[must_use]
pub fn sanitize_worktree_id_component(name: &str) -> String {
    if name == "@" {
        return "-".to_owned();
    }

    let mut out = String::new();
    let mut last = '\0';
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_ascii_control()
            || matches!(ch, ':' | '?' | '[' | '\\' | '^' | '~' | ' ' | '\t' | '*')
        {
            if (out.is_empty() && last != '-') || !out.is_empty() {
                out.push('-');
            }
            last = '-';
            i += 1;
            continue;
        }
        if ch == '.' && i + 1 < chars.len() && chars[i + 1] == '.' {
            if last == '.' {
                out.pop();
            } else {
                out.push('.');
                last = '.';
            }
            i += 2;
            continue;
        }
        if ch == '@' && i + 1 < chars.len() && chars[i + 1] == '{' {
            if let Some(last_ch) = out.pop() {
                if last_ch != '-' {
                    out.push('-');
                }
            }
            last = '-';
            i += 2;
            continue;
        }
        if ch == '.' && out.is_empty() {
            out.push('-');
            last = '-';
            i += 1;
            continue;
        }
        out.push(ch);
        last = ch;
        i += 1;
    }

    const LOCK_SUFFIX: &str = ".lock";
    while out.ends_with(LOCK_SUFFIX) {
        out.truncate(out.len() - LOCK_SUFFIX.len());
    }
    while out.ends_with('.') {
        out.pop();
    }
    out
}

/// Pick a unique `<common>/worktrees/<id>/` directory for a new linked worktree at `wt_path`.
///
/// Git uses the sanitized basename and appends `1`, `2`, … when the admin dir already exists.
#[must_use]
pub fn allocate_worktree_admin_dir(common: &Path, wt_path: &Path) -> PathBuf {
    let worktrees_dir = common.join("worktrees");
    let base = sanitize_worktree_id_component(&worktree_path_basename(wt_path));
    let base = if base.is_empty() {
        "worktree".to_owned()
    } else {
        base
    };

    let mut counter = 0u32;
    loop {
        let id = if counter == 0 {
            base.clone()
        } else {
            format!("{base}{counter}")
        };
        let admin = worktrees_dir.join(&id);
        if !admin.exists() {
            return admin;
        }
        counter = counter.saturating_add(1);
        if counter == 0 {
            break;
        }
    }
    worktrees_dir.join(format!("{base}{}", std::process::id()))
}

/// Copy `config.worktree` into a linked worktree admin dir, stripping keys Git omits
/// when `extensions.worktreeConfig` is enabled (`core.bare`, `core.worktree`).
pub fn copy_filtered_worktree_config(source_git_dir: &Path, admin_dir: &Path) -> Result<()> {
    let src = source_git_dir.join("config.worktree");
    if !src.is_file() {
        return Ok(());
    }
    let dst = admin_dir.join("config.worktree");
    fs::copy(&src, &dst).map_err(Error::Io)?;
    strip_worktree_config_keys(&dst, &["core.bare", "core.worktree"])?;
    Ok(())
}

fn strip_worktree_config_keys(path: &Path, keys: &[&str]) -> Result<()> {
    let content = fs::read_to_string(path).map_err(Error::Io)?;
    let mut kept = Vec::new();
    let mut section: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            kept.push(line);
            continue;
        }
        if trimmed.starts_with('[') {
            let end = trimmed.find(']').unwrap_or(trimmed.len());
            let name = trimmed[1..end].trim().to_ascii_lowercase();
            section = Some(name);
            kept.push(line);
            continue;
        }
        if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let full = match section.as_deref() {
                Some(sec) => format!("{sec}.{key}"),
                None => key.clone(),
            };
            if keys.iter().any(|k| full.eq_ignore_ascii_case(k)) {
                continue;
            }
        } else if keys.iter().any(|k| trimmed.eq_ignore_ascii_case(k)) {
            continue;
        }
        kept.push(line);
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(path, out).map_err(Error::Io)
}

/// How [`add_linked_worktree`] attaches `HEAD` to a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeBranch {
    /// Check out an existing local branch (its tip must match the start commit).
    Existing(String),
    /// Create `refs/heads/<name>` at the start commit (fails when the branch exists).
    Create(String),
    /// Create or reset `refs/heads/<name>` to the start commit (`git worktree add -B`).
    CreateOrReset(String),
    /// Detached HEAD at the start commit.
    Detach,
}

/// Options for [`add_linked_worktree`].
#[derive(Debug, Clone)]
pub struct AddLinkedWorktreeOptions {
    /// Directory for the new working tree (created when missing).
    pub path: PathBuf,
    /// Branch mode. When `None`, a new branch named after the path basename is created.
    pub branch: Option<WorktreeBranch>,
    /// Start commit (revision spec). Defaults to the current `HEAD` commit.
    pub start_point: Option<String>,
    /// Replace a stale registration when the working tree directory is missing.
    pub force: bool,
    /// When set, write `locked` before checkout finishes (same order as Git).
    pub lock: Option<LockWorktreeOptions>,
    /// Register the worktree and set `HEAD` without populating the working tree or index.
    pub no_checkout: bool,
    /// When the start point is a remote-tracking branch, configure upstream for the new branch.
    pub track: bool,
}

/// Options for locking during [`add_linked_worktree`] or [`RepositoryWorktrees::lock`].
#[derive(Debug, Clone, Default)]
pub struct LockWorktreeOptions {
    /// Optional reason stored in the `locked` file (shown by `git worktree list`).
    pub reason: Option<String>,
}

/// Options for [`prune_linked_worktrees`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneWorktreesOptions {
    /// List prunable worktrees without removing admin directories.
    pub dry_run: bool,
}

/// One linked worktree removed or that would be removed by [`prune_linked_worktrees`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedWorktreeEntry {
    /// Working tree path from the registry.
    pub path: PathBuf,
    /// Why the entry is prunable (matches `git worktree prune` messaging).
    pub reason: String,
}

/// Register and populate a linked worktree (Git `worktree add` layout).
///
/// # Errors
///
/// Fails when the repository is bare, the path is occupied, the branch is already
/// checked out elsewhere, or checkout cannot complete.
pub fn add_linked_worktree(
    repo: &Repository,
    opts: &AddLinkedWorktreeOptions,
) -> Result<WorktreeEntry> {
    let common = common_git_dir(repo.git_dir());
    if is_bare_repository(&common) {
        return Err(Error::Message(
            "cannot add a worktree to a bare repository".into(),
        ));
    }

    let wt_path = if opts.path.exists() {
        path_for_disk_compare(&opts.path)
    } else {
        opts.path.clone()
    };
    let admin_plan = plan_admin_dir_for_add(&common, &wt_path, opts.force)?;
    ensure_empty_or_missing(&wt_path)?;

    let start_spec = opts.start_point.as_deref().unwrap_or("HEAD");
    let start_oid = resolve_revision(repo, start_spec)?;
    let start_obj = repo.odb().read(&start_oid)?;
    if start_obj.kind != ObjectKind::Commit {
        return Err(Error::Message(format!(
            "revision '{start_spec}' does not resolve to a commit"
        )));
    }
    let tree_oid = parse_commit(&start_obj.data)?.tree;

    let head_ref = resolve_worktree_head_ref(repo, opts, &wt_path, start_spec, start_oid)?;

    let admin = admin_plan.admin_dir;
    let worktree_absent_before = !wt_path.exists();
    let mut stale_guard = admin_plan
        .stale_admins
        .map(StaleWorktreeAdminBackup::remove_and_backup)
        .transpose()?;

    let finish = (|| -> Result<WorktreeEntry> {
        fs::create_dir_all(&admin).map_err(Error::Io)?;
        if let Some(guard) = stale_guard.as_mut() {
            guard.set_new_admin(&admin);
        }
        fs::create_dir_all(&wt_path).map_err(Error::Io)?;
        if let Some(guard) = stale_guard.as_mut() {
            guard.track_replacement_worktree(&wt_path, worktree_absent_before);
        }

        let wt_git = wt_path.join(".git");
        fs::write(
            &wt_git,
            format!(
                "gitdir: {}\n",
                absolute_path_for_gitfile_stored_path(&admin)
            ),
        )
        .map_err(Error::Io)?;
        let wt_git_abs = absolute_path_for_gitfile_stored_path(&wt_git);
        fs::write(admin.join("commondir"), "../..\n").map_err(Error::Io)?;
        fs::write(admin.join("gitdir"), format!("{wt_git_abs}\n")).map_err(Error::Io)?;

        if let Some(ref branch_ref) = head_ref {
            fs::write(admin.join("HEAD"), format!("ref: {branch_ref}\n")).map_err(Error::Io)?;
        } else {
            fs::write(admin.join("HEAD"), format!("{start_oid}\n")).map_err(Error::Io)?;
        }

        copy_filtered_worktree_config(&common, &admin)?;

        if let Some(lock) = &opts.lock {
            write_admin_lock_file(&admin, lock.reason.as_deref())?;
        }

        if !opts.no_checkout {
            let linked = Repository::discover(
                Some(wt_path.as_path()),
                &repo.linked_worktree_discover_options(),
            )?;
            checkout_between_trees(&linked, None, &tree_oid)?;
        }

        linked_worktree_entry(&admin, &common)
    })();

    match finish {
        Ok(entry) => {
            if let Some(guard) = stale_guard.take() {
                guard.commit();
            }
            Ok(entry)
        }
        Err(err) => Err(err),
    }
}

/// Remove admin directories for linked worktrees whose working tree is gone.
///
/// Skips locked worktrees. Returns paths to pruned working trees (for reporting).
///
/// # Errors
///
/// Propagates I/O failures while reading or removing admin directories.
pub fn prune_linked_worktrees(
    repo: &Repository,
    opts: &PruneWorktreesOptions,
) -> Result<Vec<PrunedWorktreeEntry>> {
    let common = common_git_dir(repo.git_dir());
    let worktrees_dir = common.join("worktrees");
    if !worktrees_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut pruned = Vec::new();
    for entry in list_worktrees(repo)? {
        if entry.admin_dir == common || entry.is_locked || !entry.is_prunable {
            continue;
        }
        let reason = prune_reason(&entry.admin_dir, &entry.path);
        pruned.push(PrunedWorktreeEntry {
            path: entry.path.clone(),
            reason,
        });
        if !opts.dry_run {
            fs::remove_dir_all(&entry.admin_dir).map_err(Error::Io)?;
        }
    }
    Ok(pruned)
}

fn prune_reason(admin: &Path, wt_path: &Path) -> String {
    let prunable_file = admin.join("prunable");
    if prunable_file.is_file() {
        if let Ok(content) = fs::read_to_string(&prunable_file) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    if !wt_path.exists() {
        return "gitdir file points to non-existent location".into();
    }
    "prunable".into()
}

fn write_admin_lock_file(admin: &Path, reason: Option<&str>) -> Result<()> {
    let body = reason.unwrap_or("");
    fs::write(admin.join("locked"), body).map_err(Error::Io)
}

fn resolve_worktree_head_ref(
    repo: &Repository,
    opts: &AddLinkedWorktreeOptions,
    wt_path: &Path,
    start_spec: &str,
    start_oid: crate::objects::ObjectId,
) -> Result<Option<String>> {
    match &opts.branch {
        Some(WorktreeBranch::Detach) => Ok(None),
        Some(WorktreeBranch::Existing(name)) => {
            let branch_ref = prepare_existing_branch(repo, name, start_oid)?;
            branch_checked_out_elsewhere(repo, &branch_ref)?;
            Ok(Some(branch_ref))
        }
        Some(WorktreeBranch::Create(name)) => {
            let branch_ref = create_worktree_branch(repo, name, start_spec, start_oid, false)?;
            branch_checked_out_elsewhere(repo, &branch_ref)?;
            maybe_track_new_branch(repo, opts, name, start_spec, start_oid)?;
            Ok(Some(branch_ref))
        }
        Some(WorktreeBranch::CreateOrReset(name)) => {
            let branch_ref = create_worktree_branch(repo, name, start_spec, start_oid, true)?;
            branch_checked_out_elsewhere(repo, &branch_ref)?;
            maybe_track_new_branch(repo, opts, name, start_spec, start_oid)?;
            Ok(Some(branch_ref))
        }
        None => {
            if let Some(start) = opts.start_point.as_deref() {
                if let Some(branch_ref) = resolve_existing_branch(repo, start) {
                    let tip = refs::resolve_ref(repo.git_dir(), &branch_ref)?;
                    if tip != start_oid {
                        return Err(Error::Message(format!(
                            "branch '{}' does not match start commit",
                            branch_ref
                                .strip_prefix("refs/heads/")
                                .unwrap_or(&branch_ref)
                        )));
                    }
                    branch_checked_out_elsewhere(repo, &branch_ref)?;
                    return Ok(Some(branch_ref));
                }
                return Ok(None);
            }
            let branch = worktree_path_basename(wt_path);
            validate_branch_short_name(&branch).map_err(|e| Error::Message(e.to_string()))?;
            let branch_ref = create_worktree_branch(repo, &branch, start_spec, start_oid, false)?;
            branch_checked_out_elsewhere(repo, &branch_ref)?;
            maybe_track_new_branch(repo, opts, &branch, start_spec, start_oid)?;
            Ok(Some(branch_ref))
        }
    }
}

fn prepare_existing_branch(
    repo: &Repository,
    name: &str,
    start_oid: crate::objects::ObjectId,
) -> Result<String> {
    validate_branch_short_name(name).map_err(|e| Error::Message(e.to_string()))?;
    let branch_ref = format!("refs/heads/{name}");
    if refs::resolve_ref(repo.git_dir(), &branch_ref).is_err() {
        return Err(Error::Message(format!("no branch named '{name}'")));
    }
    let tip = refs::resolve_ref(repo.git_dir(), &branch_ref)?;
    if tip != start_oid {
        return Err(Error::Message(format!(
            "branch '{name}' does not match start commit"
        )));
    }
    Ok(branch_ref)
}

fn create_worktree_branch(
    repo: &Repository,
    name: &str,
    start_spec: &str,
    start_oid: crate::objects::ObjectId,
    force_reset: bool,
) -> Result<String> {
    validate_branch_short_name(name).map_err(|e| Error::Message(e.to_string()))?;
    let branch_ref = format!("refs/heads/{name}");
    let existing = repo.references().read_existing(&branch_ref)?;
    if existing.is_some() && !force_reset {
        return Err(Error::Message(format!("branch '{name}' already exists")));
    }
    if let Some(existing_ref) = existing {
        ensure_branch_not_checked_out_in_any_worktree(repo, name)?;
        reset_branch_ref_with_reflog(repo, &branch_ref, start_oid, start_spec, &existing_ref)?;
    } else {
        create_branch_ref_with_reflog(repo, &branch_ref, start_oid, start_spec)?;
    }
    Ok(branch_ref)
}

fn maybe_track_new_branch(
    repo: &Repository,
    opts: &AddLinkedWorktreeOptions,
    branch_short: &str,
    start_spec: &str,
    start_oid: crate::objects::ObjectId,
) -> Result<()> {
    if !opts.track {
        return Ok(());
    }
    let track = TrackMode::Always;
    let Some((remote, merge)) =
        resolve_create_tracking(repo, branch_short, start_spec, start_oid, track)?
    else {
        return Ok(());
    };
    write_branch_upstream_config(repo, branch_short, &remote, &merge)
}

fn linked_worktree_entry(admin: &Path, common: &Path) -> Result<WorktreeEntry> {
    let wt_path = read_worktree_path(admin)?;
    let wt_head = resolve_linked_head(admin, common);
    let (is_locked, lock_reason) = read_lock_state(admin)?;
    let is_prunable = worktree_entry_is_prunable(admin, &wt_path, is_locked);
    Ok(WorktreeEntry {
        path: wt_path,
        head: wt_head,
        is_bare: false,
        is_locked,
        lock_reason,
        admin_dir: admin.to_path_buf(),
        is_prunable,
    })
}

fn worktree_entry_is_prunable(admin: &Path, wt_path: &Path, is_locked: bool) -> bool {
    if is_locked {
        return false;
    }
    if admin.join("prunable").is_file() {
        return true;
    }
    !wt_path.exists()
}

/// Resolve `path` for [`add_linked_worktree`].
///
/// Relative paths are interpreted from `relative_to` (typically the process working
/// directory at the CLI boundary). The result is absolute and stable for registry
/// comparison even when the leaf directory does not exist yet.
///
/// # Errors
///
/// Returns [`Error::Message`] when `path` has no final component.
pub fn resolve_linked_worktree_add_path(path: &Path, relative_to: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path_for_disk_compare(path));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        relative_to.join(path)
    };
    if let Some(parent) = absolute.parent().filter(|p| p.exists()) {
        let Some(name) = absolute.file_name() else {
            return Err(Error::Message("invalid worktree path".into()));
        };
        let mut resolved = path_for_disk_compare(parent);
        resolved.push(name);
        return Ok(resolved);
    }
    Ok(absolute)
}

struct WorktreeAdminAddPlan {
    admin_dir: PathBuf,
    stale_admins: Option<Vec<PathBuf>>,
}

/// On failure after stale admin dirs were moved aside, restore them.
struct StaleWorktreeAdminBackup {
    moved_aside: Vec<(PathBuf, PathBuf)>,
    new_admin: Option<PathBuf>,
    replacement_worktree: Option<(PathBuf, bool)>,
}

impl StaleWorktreeAdminBackup {
    fn remove_and_backup(stale_admins: Vec<PathBuf>) -> Result<Self> {
        let mut moved_aside = Vec::new();
        for (i, admin) in stale_admins.into_iter().enumerate() {
            let id = admin
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "wt".to_owned());
            let aside = admin
                .parent()
                .ok_or_else(|| Error::Message("invalid worktree admin path".into()))?
                .join(format!(".{id}-stale-backup-{i}"));
            if aside.exists() {
                fs::remove_dir_all(&aside).map_err(Error::Io)?;
            }
            fs::rename(&admin, &aside).map_err(Error::Io)?;
            moved_aside.push((admin, aside));
        }
        Ok(Self {
            moved_aside,
            new_admin: None,
            replacement_worktree: None,
        })
    }

    fn set_new_admin(&mut self, admin: &Path) {
        self.new_admin = Some(admin.to_path_buf());
    }

    fn track_replacement_worktree(&mut self, path: &Path, absent_before_replacement: bool) {
        self.replacement_worktree = Some((path.to_path_buf(), absent_before_replacement));
    }

    fn commit(mut self) {
        for (_, aside) in self.moved_aside.drain(..) {
            let _ = fs::remove_dir_all(aside);
        }
    }
}

impl Drop for StaleWorktreeAdminBackup {
    fn drop(&mut self) {
        if self.moved_aside.is_empty() {
            return;
        }
        if let Some((wt_path, absent_before)) = self.replacement_worktree.take() {
            if absent_before {
                let _ = fs::remove_dir_all(&wt_path);
            } else if wt_path.join(".git").is_file() {
                let _ = fs::remove_file(wt_path.join(".git"));
            }
        }
        if let Some(new_admin) = self.new_admin.take() {
            let _ = fs::remove_dir_all(new_admin);
        }
        for (admin, aside) in &self.moved_aside {
            let _ = fs::remove_dir_all(admin);
            let _ = fs::rename(aside, admin);
        }
    }
}

fn linked_admins_for_worktree_path(common: &Path, wt_path: &Path) -> Result<Vec<PathBuf>> {
    let target = path_for_disk_compare(wt_path);
    let worktrees_dir = common.join("worktrees");
    if !worktrees_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut admins = Vec::new();
    for entry in fs::read_dir(&worktrees_dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let admin = entry.path();
        let registered = read_worktree_path(&admin)?;
        if path_for_disk_compare(&registered) == target {
            admins.push(admin);
        }
    }
    Ok(admins)
}

fn plan_admin_dir_for_add(
    common: &Path,
    wt_path: &Path,
    force: bool,
) -> Result<WorktreeAdminAddPlan> {
    let existing = linked_admins_for_worktree_path(common, wt_path)?;
    if existing.is_empty() {
        return Ok(WorktreeAdminAddPlan {
            admin_dir: allocate_worktree_admin_dir(common, wt_path),
            stale_admins: None,
        });
    }

    let display = wt_path.display().to_string();
    if wt_path.exists() {
        return Err(Error::WorktreePathRegistered { path: display });
    }
    if !force {
        return Err(Error::WorktreePathRegisteredMissing { path: display });
    }

    let worktrees_dir = common.join("worktrees");
    let base = sanitize_worktree_id_component(&worktree_path_basename(wt_path));
    let base = if base.is_empty() {
        "worktree".to_owned()
    } else {
        base
    };
    let preferred = worktrees_dir.join(&base);
    let admin_dir = existing
        .iter()
        .find(|admin| {
            admin
                .file_name()
                .is_some_and(|name| name.to_string_lossy() == base)
        })
        .cloned()
        .unwrap_or_else(|| {
            if preferred.exists() {
                allocate_worktree_admin_dir(common, wt_path)
            } else {
                preferred
            }
        });
    Ok(WorktreeAdminAddPlan {
        admin_dir,
        stale_admins: Some(existing),
    })
}

/// Remove a linked worktree and its admin directory.
///
/// # Errors
///
/// Refuses to remove the main worktree, locked worktrees (unless `force`), or dirty
/// trees (unless `force`).
pub fn remove_linked_worktree(repo: &Repository, path: &Path, force: bool) -> Result<()> {
    let common = common_git_dir(repo.git_dir());
    let target = path_for_disk_compare(path);
    let entry = list_worktrees(repo)?
        .into_iter()
        .find(|e| e.path == target)
        .ok_or_else(|| {
            Error::Message(format!(
                "'{}' is not a registered worktree",
                target.display()
            ))
        })?;
    if entry.admin_dir == common {
        return Err(Error::Message("cannot remove the main working tree".into()));
    }
    if entry.is_locked && !force {
        return Err(Error::Message(
            entry
                .lock_reason
                .map(|r| format!("worktree is locked: {r}"))
                .unwrap_or_else(|| "worktree is locked".into()),
        ));
    }

    let linked = Repository::discover(
        Some(entry.path.as_path()),
        &repo.linked_worktree_discover_options(),
    )?;
    if !force && worktree_has_local_changes(&linked)? {
        return Err(Error::Message(
            "worktree contains modified or untracked files; use -f or --force to remove".into(),
        ));
    }

    fs::remove_dir_all(&entry.admin_dir).map_err(Error::Io)?;
    if entry.path.exists() {
        fs::remove_dir_all(&entry.path).map_err(Error::Io)?;
    }
    Ok(())
}

fn resolve_existing_branch(repo: &Repository, spec: &str) -> Option<String> {
    let full = if spec.starts_with("refs/heads/") {
        spec.to_owned()
    } else {
        format!("refs/heads/{spec}")
    };
    refs::resolve_ref(repo.git_dir(), &full).ok()?;
    Some(full)
}

fn branch_checked_out_elsewhere(repo: &Repository, branch_ref: &str) -> Result<()> {
    let resolved = branch_ref.to_owned();
    for wt in list_worktrees(repo)? {
        if let HeadState::Branch { refname, .. } = &wt.head {
            if refname == &resolved {
                return Err(Error::Message(format!(
                    "branch '{}' is already checked out at '{}'",
                    resolved.strip_prefix("refs/heads/").unwrap_or(&resolved),
                    wt.path.display()
                )));
            }
        }
    }
    Ok(())
}

fn ensure_empty_or_missing(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Err(Error::Message(format!(
            "'{}' already exists and is not a directory",
            path.display()
        )));
    }
    let mut entries = fs::read_dir(path).map_err(Error::Io)?;
    if entries.next().is_some() {
        return Err(Error::Message(format!(
            "'{}' already exists",
            path.display()
        )));
    }
    Ok(())
}

fn worktree_has_local_changes(repo: &Repository) -> Result<bool> {
    let model = status(repo, &StatusOptions::default(), &mut NullProgress)?;
    Ok(!model.staged.is_empty()
        || !model.unstaged.is_empty()
        || !model.conflicts.is_empty()
        || !model.untracked.is_empty())
}

/// Read the working tree path from `<admin>/gitdir` (parent of the worktree `.git` file).
pub fn read_worktree_path(admin: &Path) -> Result<PathBuf> {
    let gitdir_path = admin.join("gitdir");
    if !gitdir_path.is_file() {
        return Ok(admin.to_path_buf());
    }
    let raw = fs::read_to_string(&gitdir_path).map_err(Error::Io)?;
    let mut p = PathBuf::from(raw.trim());
    if p.is_relative() {
        p = admin.join(p);
    }
    let parent = p.parent().unwrap_or(&p).to_path_buf();
    Ok(path_for_disk_compare(&parent))
}

fn read_lock_state(admin: &Path) -> Result<(bool, Option<String>)> {
    let locked_file = admin.join("locked");
    if !locked_file.is_file() {
        return Ok((false, None));
    }
    let content = fs::read_to_string(&locked_file).map_err(Error::Io)?;
    let reason = content.trim();
    if reason.is_empty() {
        Ok((true, None))
    } else {
        Ok((true, Some(reason.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::Repository;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn list_main_worktree_only() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::write(
            root.join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .unwrap();

        let repo = Repository::open(
            &root.join(".git"),
            &RepositoryOptions::empty().with_work_tree(&root),
        )
        .unwrap();
        let list = list_worktrees(&repo).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, path_for_disk_compare(&root));
        assert!(!list[0].is_bare);
    }

    #[test]
    fn allocate_unique_worktree_id() {
        let tmp = TempDir::new().unwrap();
        let common = tmp.path().join("git");
        fs::create_dir_all(common.join("worktrees/here")).unwrap();
        let admin = allocate_worktree_admin_dir(&common, Path::new("/tmp/sub/here"));
        assert_eq!(admin, common.join("worktrees/here1"));
    }

    #[test]
    fn strip_worktree_config_removes_core_bare_and_worktree() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.worktree");
        fs::write(
            &path,
            "[core]\n\tbare = true\n\tworktree = /wt\n[bogus]\n\tkey = value\n",
        )
        .unwrap();
        strip_worktree_config_keys(&path, &["core.bare", "core.worktree"]).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("bogus"));
        assert!(!out.contains("bare"));
        assert!(!out.contains("worktree"));
    }

    #[test]
    fn sanitize_funny_worktree_name() {
        assert_eq!(
            sanitize_worktree_id_component(".  weird*..?.lock.lock"),
            "---weird-.-"
        );
    }

    #[test]
    fn read_worktree_path_from_gitdir_file() {
        let tmp = TempDir::new().unwrap();
        let admin = tmp.path().join("wt-admin");
        fs::create_dir_all(&admin).unwrap();
        let wt = tmp.path().join("linked");
        fs::create_dir_all(wt.join(".git")).unwrap();
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        let path = read_worktree_path(&admin).unwrap();
        assert_eq!(path, path_for_disk_compare(&wt));
    }

    #[test]
    fn add_and_remove_linked_worktree_via_library() {
        use std::process::Command;
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(main.join("t"), b"1\n").unwrap();
        git(&["add", "t"]);
        git(&["commit", "-qm", "init"]);

        let repo = Repository::discover(Some(main.as_path()), &RepositoryOptions::empty()).unwrap();
        let linked = tmp.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked.clone(),
                branch: Some(WorktreeBranch::Create("wt".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .unwrap();
        assert_eq!(list_worktrees(&repo).unwrap().len(), 2);
        remove_linked_worktree(&repo, &linked, false).unwrap();
        assert_eq!(list_worktrees(&repo).unwrap().len(), 1);
    }

    #[test]
    fn registered_worktree_count_and_basename() {
        let tmp = TempDir::new().unwrap();
        let common = tmp.path().join("git");
        fs::create_dir_all(common.join("worktrees/a")).unwrap();
        fs::create_dir_all(common.join("worktrees/b")).unwrap();
        assert_eq!(registered_worktree_count(&common), 3);
        assert_eq!(
            worktree_path_basename(Path::new("/tmp/my/project/")),
            "project"
        );
    }

    #[test]
    fn registered_worktree_count_without_worktrees_dir() {
        let tmp = TempDir::new().unwrap();
        let common = tmp.path().join("only-main");
        fs::create_dir_all(&common).unwrap();
        assert_eq!(registered_worktree_count(&common), 1);
    }

    #[test]
    fn common_git_dir_and_bare_detection() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join("bare.git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
        )
        .unwrap();
        assert!(is_bare_repository(&git_dir));
        assert_eq!(common_git_dir(&git_dir), git_dir);
    }

    #[test]
    fn sanitize_worktree_id_special_cases() {
        assert_eq!(sanitize_worktree_id_component("@"), "-");
        assert!(!sanitize_worktree_id_component("@{upstream}").contains('@'));
        assert_eq!(sanitize_worktree_id_component("..name"), ".name");
        assert_eq!(sanitize_worktree_id_component(".lock.lock"), "-lock");
        let tmp = TempDir::new().unwrap();
        let common = tmp.path().join("git");
        fs::create_dir_all(&common).unwrap();
        assert_eq!(
            allocate_worktree_admin_dir(&common, Path::new("/")),
            common.join("worktrees/worktree")
        );
    }

    #[test]
    fn copy_filtered_worktree_config_no_source_is_ok() {
        let tmp = TempDir::new().unwrap();
        let admin = tmp.path().join("admin");
        fs::create_dir_all(&admin).unwrap();
        copy_filtered_worktree_config(tmp.path(), &admin).unwrap();
        assert!(!admin.join("config.worktree").exists());
    }

    #[test]
    fn copy_filtered_worktree_config_copies_file() {
        let tmp = TempDir::new().unwrap();
        let common = tmp.path().join("common");
        let admin = tmp.path().join("admin");
        fs::create_dir_all(&common).unwrap();
        fs::create_dir_all(&admin).unwrap();
        fs::write(
            common.join("config.worktree"),
            "[core]\n\tbare = false\n\tworktree = /x\n[ext]\n\tk = v\n",
        )
        .unwrap();
        copy_filtered_worktree_config(&common, &admin).unwrap();
        let out = fs::read_to_string(admin.join("config.worktree")).unwrap();
        assert!(out.contains("[ext]"));
        assert!(!out.contains("worktree"));
    }

    #[test]
    fn read_lock_state_and_missing_gitdir_path() {
        let tmp = TempDir::new().unwrap();
        let admin = tmp.path().join("admin");
        fs::create_dir_all(&admin).unwrap();
        assert_eq!(read_worktree_path(&admin).unwrap(), admin);
        fs::write(admin.join("locked"), "maintenance\n").unwrap();
        let (locked, reason) = read_lock_state(&admin).unwrap();
        assert!(locked);
        assert_eq!(reason.as_deref(), Some("maintenance"));
    }

    #[test]
    fn ensure_empty_or_missing_rejects_nonempty_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("occupied");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("file"), b"x").unwrap();
        assert!(ensure_empty_or_missing(&dir).is_err());
        let missing = tmp.path().join("missing");
        assert!(ensure_empty_or_missing(&missing).is_ok());
    }

    #[test]
    fn add_linked_worktree_rejects_bare_repository() {
        let tmp = TempDir::new().unwrap();
        let bare = tmp.path().join("bare.git");
        fs::create_dir_all(bare.join("objects")).unwrap();
        fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            bare.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
        )
        .unwrap();
        let repo = Repository::open(&bare, &RepositoryOptions::empty()).unwrap();
        let err = add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: tmp.path().join("wt"),
                branch: Some(WorktreeBranch::Create("wt".into())),
                start_point: None,
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Message(_)));
    }

    #[test]
    fn list_worktrees_from_linked_checkout_normalizes_main_path() {
        use std::process::Command;
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(main.join("t"), b"1\n").unwrap();
        git(&["add", "t"]);
        git(&["commit", "-qm", "init"]);

        let repo = Repository::discover(Some(main.as_path()), &RepositoryOptions::empty()).unwrap();
        let linked = tmp.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked.clone(),
                branch: Some(WorktreeBranch::Create("wt".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .unwrap();

        let linked_repo =
            Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).unwrap();
        let list = list_worktrees(&linked_repo).unwrap();
        assert_eq!(list.len(), 2);
        let expected_main = path_for_disk_compare(&main);
        assert_eq!(list[0].path, expected_main);
        #[cfg(windows)]
        {
            let displayed = list[0].path.display().to_string();
            assert!(
                !displayed.starts_with(r"\\?\"),
                "main worktree path must not use the verbatim prefix: {displayed}"
            );
        }
    }

    #[test]
    fn branch_checkout_exclusivity_helpers() {
        use std::process::Command;
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(main.join("t"), b"1\n").unwrap();
        git(&["add", "t"]);
        git(&["commit", "-qm", "init"]);

        let repo = Repository::discover(Some(main.as_path()), &RepositoryOptions::empty()).unwrap();
        let linked = tmp.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked.clone(),
                branch: Some(WorktreeBranch::Create("feature".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .unwrap();

        let linked_repo =
            Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty()).unwrap();
        let linked_admin = linked_repo
            .git_dir()
            .canonicalize()
            .unwrap_or_else(|_| linked_repo.git_dir().to_path_buf());
        assert_eq!(worktree_admin_dir(&linked_repo), linked_admin);

        let found = find_worktree_with_branch_checked_out(&repo, "feature", None)
            .unwrap()
            .expect("linked checkout");
        assert_eq!(found.branch, "feature");

        assert!(ensure_branch_not_checked_out_in_any_worktree(&repo, "feature").is_err());
        assert!(ensure_branch_not_checked_out_elsewhere(&linked_repo, "feature").is_ok());
        assert!(ensure_branch_not_checked_out_elsewhere(&repo, "feature").is_err());
    }

    #[test]
    fn missing_registered_path_rejects_add_without_force() {
        use std::process::Command;
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(main.join("t"), b"1\n").unwrap();
        git(&["add", "t"]);
        git(&["commit", "-qm", "init"]);

        let repo =
            Repository::discover(Some(&main), &crate::environment::RepositoryOptions::empty())
                .unwrap();
        let linked = tmp.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked.clone(),
                branch: Some(WorktreeBranch::Create("a".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .unwrap();
        fs::remove_dir_all(&linked).unwrap();
        assert!(matches!(
            add_linked_worktree(
                &repo,
                &AddLinkedWorktreeOptions {
                    path: linked.clone(),
                    branch: Some(WorktreeBranch::Create("b".into())),
                    start_point: Some("HEAD".into()),
                    force: false,
                    lock: None,
                    no_checkout: false,
                    track: false,
                },
            )
            .unwrap_err(),
            Error::WorktreePathRegisteredMissing { .. }
        ));
    }

    #[test]
    fn remove_linked_worktree_refuses_main_and_locked() {
        use std::process::Command;
        let tmp = TempDir::new().unwrap();
        let main = tmp.path().join("main");
        fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .current_dir(&main)
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q", "-b", "main"]);
        fs::write(main.join("t"), b"1\n").unwrap();
        git(&["add", "t"]);
        git(&["commit", "-qm", "init"]);

        let repo = Repository::discover(Some(main.as_path()), &RepositoryOptions::empty()).unwrap();
        assert!(remove_linked_worktree(&repo, &main, false).is_err());

        let linked = tmp.path().join("linked");
        add_linked_worktree(
            &repo,
            &AddLinkedWorktreeOptions {
                path: linked.clone(),
                branch: Some(WorktreeBranch::Create("wt".into())),
                start_point: Some("HEAD".into()),
                force: false,
                lock: None,
                no_checkout: false,
                track: false,
            },
        )
        .unwrap();
        let common = common_git_dir(repo.git_dir());
        let admin = fs::read_dir(common.join("worktrees"))
            .unwrap()
            .find_map(|e| e.ok())
            .map(|e| e.path())
            .expect("admin");
        fs::write(admin.join("locked"), "hold\n").unwrap();
        assert!(remove_linked_worktree(&repo, &linked, false).is_err());
        assert!(remove_linked_worktree(&repo, &linked, true).is_ok());
    }
}
