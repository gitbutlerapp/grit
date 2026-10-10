//! Stash create/store/list/drop/apply/pop/show primitives.
//!
//! The library builds Git-compatible stash commits (W/I/U parents), updates
//! `refs/stash` and its reflog, and applies entries onto the worktree and index.
//! The `grit` binary keeps argument parsing, human/`--json`/`--markdown`
//! output, and exit-code mapping; [`apply_stash`] and [`pop_stash`] return
//! whether merge conflicts occurred so the CLI can decide whether to drop.
//!
//! Flattening and worktree helpers ([`FlatTreeEntry`], [`flatten_tree_full`],
//! [`add_stage_entry`], [`worktree_bytes_for_index_mode`],
//! [`write_regular_file_replacing_symlink`], [`remove_empty_dirs`]) are shared
//! building blocks for apply and for embedders that manipulate stash trees.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, DiffEntry,
    DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, Index, IndexEntry, MODE_GITLINK, MODE_SYMLINK};
// `MODE_EXECUTABLE` only drives the Unix executable-bit application below.
#[cfg(unix)]
use crate::index::MODE_EXECUTABLE;
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::commit::write_commit_object;
use crate::porcelain::merge::tree_to_index_entries;
use crate::porcelain::status::{
    collect_untracked_and_ignored_inner, expand_untracked_for_staging_with_rules, IgnoredMode,
    UntrackedScan,
};
use crate::reflog::{delete_reflog_entries_rechain, read_reflog, truncate_last_reflog_line};
use crate::refs::{self, append_reflog, delete_ref, write_ref};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

/// A single blob entry from a recursively flattened tree.
#[derive(Clone)]
pub struct FlatTreeEntry {
    pub path: String,
    pub mode: u32,
    pub oid: ObjectId,
}

/// Recursively flatten a tree into (path, mode, oid) entries.
pub fn flatten_tree_full(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<FlatTreeEntry>> {
    let obj = odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();
    for entry in entries {
        let entry_name = String::from_utf8_lossy(&entry.name).to_string();
        let full_path = if prefix.is_empty() {
            entry_name
        } else {
            format!("{prefix}/{entry_name}")
        };
        if entry.mode == 0o40000 {
            let sub = flatten_tree_full(odb, &entry.oid, &full_path)?;
            result.extend(sub);
        } else {
            result.push(FlatTreeEntry {
                path: full_path,
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }
    Ok(result)
}

/// Lookup a path in a lexicographically sorted [`flatten_tree_full`] slice.
fn flat_tree_lookup<'a>(entries: &'a [FlatTreeEntry], path: &str) -> Option<&'a FlatTreeEntry> {
    entries
        .binary_search_by(|e| e.path.as_str().cmp(path))
        .ok()
        .map(|i| &entries[i])
}

/// Paths whose blob/symlink/gitlink differs between two sorted flat trees.
///
/// `None` means the path was removed in `other`; `Some` is the entry in `other`.
fn collect_flat_tree_worktree_changes<'a>(
    base: &'a [FlatTreeEntry],
    other: &'a [FlatTreeEntry],
) -> Vec<(String, Option<&'a FlatTreeEntry>)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < base.len() || j < other.len() {
        match (base.get(i), other.get(j)) {
            (Some(b), Some(o)) => match b.path.cmp(&o.path) {
                Ordering::Less => {
                    out.push((b.path.clone(), None));
                    i += 1;
                }
                Ordering::Greater => {
                    out.push((o.path.clone(), Some(o)));
                    j += 1;
                }
                Ordering::Equal => {
                    if b.oid != o.oid || b.mode != o.mode {
                        out.push((o.path.clone(), Some(o)));
                    }
                    i += 1;
                    j += 1;
                }
            },
            (Some(b), None) => {
                out.push((b.path.clone(), None));
                i += 1;
            }
            (None, Some(o)) => {
                out.push((o.path.clone(), Some(o)));
                j += 1;
            }
            (None, None) => break,
        }
    }
    out
}

/// Push a conflict (non-zero) stage entry for `path` into `index`.
pub fn add_stage_entry(index: &mut Index, path: &[u8], oid: &ObjectId, mode: u32, stage: u16) {
    let name_len = path.len().min(0xFFF) as u16;
    let flags = (stage << 12) | name_len;
    index.push_entry_unsorted(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: *oid,
        flags,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    });
}

/// Read the worktree bytes for `path`, honoring a symlink index mode (returns
/// the link target rather than following it).
pub fn worktree_bytes_for_index_mode(path: &Path, mode: u32) -> io::Result<Vec<u8>> {
    if mode == MODE_SYMLINK {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return Ok(target.as_os_str().as_bytes().to_vec());
        }
        #[cfg(not(unix))]
        {
            return Ok(target.to_string_lossy().as_bytes().to_vec());
        }
    }
    fs::read(path)
}

/// Write a regular file at `path`, first removing any pre-existing symlink there.
pub fn write_regular_file_replacing_symlink(path: &Path, contents: &[u8]) -> io::Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        fs::remove_file(path)?;
    }
    fs::write(path, contents)
}

/// Remove now-empty directories from `dir` upward toward (but not including)
/// `stop_at`, refusing to remove a directory that contains the process CWD.
pub fn remove_empty_dirs(
    dir: &Path,
    stop_at: &Path,
    environment: &crate::environment::Environment,
) {
    let cwd_rel = crate::worktree_cwd::process_cwd_repo_relative(stop_at, environment);
    let mut current = dir.to_path_buf();
    while current != stop_at {
        if fs::read_dir(&current)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            if let Some(ref cr) = cwd_rel {
                if crate::worktree_cwd::cwd_would_be_removed_with_dir(stop_at, &current, cr) {
                    break;
                }
            }
            let _ = fs::remove_dir(&current);
            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
            } else {
                break;
            }
        } else {
            break;
        }
    }
}

/// Options for [`create_stash`] and [`push_stash`].
#[derive(Debug, Clone)]
pub struct StashCreateOptions {
    /// Reflog/stash message override; when set, the W commit message is `On <branch>: <msg>`.
    pub message: Option<String>,
    /// When true, include untracked files as a parentless U commit (parent 3 of W).
    pub include_untracked: bool,
    /// Full author/committer identity (`Name <email> timestamp tz`).
    pub identity: String,
}

/// One entry from [`list_stashes`] (`stash@{{index}}` in newest-first order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashEntry {
    /// Index matching `stash@{{index}}` (0 is newest).
    pub index: usize,
    /// The W commit object id stored in the reflog.
    pub oid: ObjectId,
    /// Reflog message (what `git stash list` prints after the colon).
    pub message: String,
}

/// Build a stage-0 index whose entries match `tree_oid`.
fn index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Index> {
    let entries = tree_to_index_entries(repo, tree_oid, "")?;
    let mut index = Index::new();
    for entry in entries {
        index.push_entry_unsorted(entry);
    }
    index.sort();
    Ok(index)
}

fn head_branch_label(head: &HeadState) -> String {
    match head {
        HeadState::Branch { short_name, .. } => short_name.clone(),
        _ => "(no branch)".to_owned(),
    }
}

fn head_commit_oid(head: &HeadState) -> Result<ObjectId> {
    head.oid().copied().ok_or(Error::StashNoInitialCommit)
}

fn head_commit_subject(repo: &Repository, head_oid: &ObjectId) -> Result<String> {
    let obj = repo.odb.read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit
        .message
        .lines()
        .next()
        .unwrap_or("")
        .trim_end()
        .to_owned())
}

fn abbrev_head_oid(oid: &ObjectId) -> String {
    oid.to_hex()[..7.min(oid.to_hex().len())].to_owned()
}

fn stash_context_message(
    repo: &Repository,
    head: &HeadState,
    head_oid: &ObjectId,
) -> Result<String> {
    let branch = head_branch_label(head);
    let short = abbrev_head_oid(head_oid);
    let subject = head_commit_subject(repo, head_oid)?;
    Ok(format!("{branch}: {short} {subject}"))
}

fn has_stashable_changes(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    include_untracked: bool,
) -> Result<bool> {
    let mut index = repo.load_index()?;
    if !diff_index_to_tree(&repo.odb, &index, Some(head_tree), true)?.is_empty() {
        return Ok(true);
    }
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: true,
        repository_git_dir: Some(repo.git_dir.clone()),
        config: repo.config().ok(),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(&repo.odb, &mut index, work_tree, diff_opts)?;
    if !unstaged.is_empty() {
        return Ok(true);
    }
    if !include_untracked {
        return Ok(false);
    }
    let worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let (untracked, _) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::No,
            show_all: false,
            sort_paths: true,
            use_untracked_cache: false,
        },
        &worktree_rules,
    )?;
    Ok(!untracked.is_empty())
}

fn collect_untracked_paths(repo: &Repository, work_tree: &Path) -> Result<Vec<String>> {
    let mut index = repo.load_index()?;
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let (untracked, _) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::No,
            show_all: false,
            sort_paths: true,
            use_untracked_cache: false,
        },
        &worktree_rules,
    )?;
    expand_untracked_for_staging_with_rules(
        repo,
        &mut index,
        work_tree,
        untracked,
        &[],
        &worktree_rules,
    )
}

fn apply_worktree_diff_to_index(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
) -> Result<()> {
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: true,
        repository_git_dir: Some(repo.git_dir.clone()),
        config: repo.config().ok(),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(&repo.odb, index, work_tree, diff_opts)?;
    let mut worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let precompose = crate::precompose_config::effective_core_precomposeunicode_with_config(
        Some(&repo.git_dir),
        Some(&worktree_rules.config_arc()),
    );
    for entry in unstaged {
        let path = entry.path();
        if entry.status == DiffStatus::Deleted {
            index.remove(path.as_bytes());
            continue;
        }
        stage_tracked_path_for_stash(
            repo,
            work_tree,
            path,
            index,
            index_mtime,
            &mut worktree_rules,
            precompose,
        )?;
    }
    Ok(())
}

fn stage_tracked_path_for_stash(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    index: &mut Index,
    index_mtime: Option<(u32, u32)>,
    rules: &mut crate::worktree_rules::WorktreeRules,
    precompose_unicode: bool,
) -> Result<()> {
    use crate::diff::{
        classify_worktree_entry_for_add, mode_from_metadata, WorktreeAddRefresh,
        WorktreeAddRefreshParams,
    };
    use crate::unicode_normalization::resolve_worktree_path_for_staging;

    let conv = rules.conversion().clone();
    let resolved = resolve_worktree_path_for_staging(work_tree, rel_path, precompose_unicode);
    let abs = resolved.abs;
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            index.remove(rel_path.as_bytes());
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let staged_mode = mode_from_metadata(&meta);
    let Some(ie) = index.get(rel_path.as_bytes(), 0).cloned() else {
        return stage_untracked_path_for_stash(
            repo,
            work_tree,
            rel_path,
            index,
            precompose_unicode,
            rules,
        );
    };
    let file_attrs = rules.file_attrs(rel_path, false);
    let refresh = classify_worktree_entry_for_add(&WorktreeAddRefreshParams {
        odb: &repo.odb,
        ie: &ie,
        meta: &meta,
        abs_path: &abs,
        rel_path,
        conv: &conv,
        file_attrs: &file_attrs,
        index_mtime,
        staged_mode,
        filter_process: Some(rules.filter_process()),
    })?;
    match refresh {
        WorktreeAddRefresh::UpToDate => Ok(()),
        WorktreeAddRefresh::ModeOnly { mode } => {
            if let Some(entry) = index.get_mut(rel_path.as_bytes(), 0) {
                entry.mode = mode;
            }
            Ok(())
        }
        WorktreeAddRefresh::StatOnly => {
            let updated =
                crate::index::entry_from_metadata(&meta, rel_path.as_bytes(), ie.oid, ie.mode);
            index.add_or_replace(updated);
            Ok(())
        }
        WorktreeAddRefresh::NeedsRestage => stage_untracked_path_for_stash(
            repo,
            work_tree,
            rel_path,
            index,
            precompose_unicode,
            rules,
        ),
    }
}

fn stage_untracked_path_for_stash(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    index: &mut Index,
    precompose_unicode: bool,
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<()> {
    use crate::diff::mode_from_metadata;
    use crate::index::entry_from_stat;
    use crate::unicode_normalization::resolve_worktree_path_for_staging;

    if rel_path.ends_with('/') {
        return Ok(());
    }
    let resolved = resolve_worktree_path_for_staging(work_tree, rel_path, precompose_unicode);
    let abs = resolved.abs;
    let index_relpath = resolved.index_relpath;
    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
    if meta.is_dir() {
        return Ok(());
    }
    let mode = mode_from_metadata(&meta);
    let data = if mode == MODE_SYMLINK {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            fs::read_link(&abs)?.as_os_str().as_bytes().to_vec()
        }
        #[cfg(not(unix))]
        {
            fs::read_link(&abs)?.to_string_lossy().as_bytes().to_vec()
        }
    } else {
        fs::read(&abs)?
    };
    let oid = repo.odb.write(ObjectKind::Blob, &data)?;
    let entry = entry_from_stat(&abs, index_relpath.as_bytes(), oid, mode)?;
    index.add_or_replace(entry);
    let _ = rules;
    Ok(())
}

fn write_stash_tree_from_index(repo: &Repository, index: &mut Index) -> Result<ObjectId> {
    index.sort();
    write_tree_update_index(&repo.odb, index, "", WriteTreeFlags::silent())
}

fn write_untracked_stash_commit(
    repo: &Repository,
    work_tree: &Path,
    paths: &[String],
    context_msg: &str,
    identity: &str,
) -> Result<ObjectId> {
    let mut index = Index::new();
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let precompose = crate::precompose_config::effective_core_precomposeunicode_with_config(
        Some(&repo.git_dir),
        Some(&worktree_rules.config_arc()),
    );
    for path in paths {
        stage_untracked_path_for_stash(
            repo,
            work_tree,
            path,
            &mut index,
            precompose,
            &worktree_rules,
        )?;
    }
    let tree = write_stash_tree_from_index(repo, &mut index)?;
    let message = format!("untracked files on {context_msg}\n");
    let data = CommitData {
        tree,
        parents: Vec::new(),
        author: identity.to_owned(),
        committer: identity.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message,
        raw_message: None,
        extra_headers: Vec::new(),
    };
    write_commit_object(repo, &data, None)
}

fn remove_stashed_untracked(repo: &Repository, work_tree: &Path, paths: &[String]) -> Result<()> {
    for path in paths {
        let file_path = work_tree.join(path);
        if file_path.is_dir() {
            let _ = fs::remove_dir_all(&file_path);
        } else if file_path.exists() || file_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&file_path);
        }
        if let Some(parent) = file_path.parent() {
            remove_empty_dirs(parent, work_tree, repo.environment());
        }
    }
    Ok(())
}

struct BuiltStash {
    w_commit: ObjectId,
    w_tree: ObjectId,
    message: String,
    untracked_paths: Vec<String>,
}

fn build_stash_commit(
    repo: &Repository,
    work_tree: &Path,
    options: &StashCreateOptions,
) -> Result<Option<BuiltStash>> {
    let head = resolve_head(&repo.git_dir)?;
    let head_oid = head_commit_oid(&head)?;
    let head_obj = repo.odb.read(&head_oid)?;
    let head_commit = parse_commit(&head_obj.data)?;
    let head_tree = head_commit.tree;

    if !has_stashable_changes(repo, work_tree, &head_tree, options.include_untracked)? {
        return Ok(None);
    }

    let context = stash_context_message(repo, &head, &head_oid)?;
    let mut index = repo.load_index()?;
    let i_tree = write_stash_tree_from_index(repo, &mut index)?;

    let index_msg = format!("index on {context}\n");
    let i_commit_data = CommitData {
        tree: i_tree,
        parents: vec![head_oid],
        author: options.identity.clone(),
        committer: options.identity.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: index_msg,
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let i_commit = write_commit_object(repo, &i_commit_data, None)?;

    let untracked_paths = if options.include_untracked {
        collect_untracked_paths(repo, work_tree)?
    } else {
        Vec::new()
    };
    let u_commit = if untracked_paths.is_empty() {
        None
    } else {
        Some(write_untracked_stash_commit(
            repo,
            work_tree,
            &untracked_paths,
            &context,
            &options.identity,
        )?)
    };

    let mut w_index = index_from_tree(repo, &i_tree)?;
    apply_worktree_diff_to_index(repo, work_tree, &mut w_index)?;
    let w_tree = write_stash_tree_from_index(repo, &mut w_index)?;

    let w_message = match &options.message {
        Some(msg) => format!("On {}: {}\n", head_branch_label(&head), msg),
        None => format!("WIP on {context}\n"),
    };

    let mut parents = vec![head_oid, i_commit];
    if let Some(u) = u_commit {
        parents.push(u);
    }
    let w_commit_data = CommitData {
        tree: w_tree,
        parents,
        author: options.identity.clone(),
        committer: options.identity.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: w_message.clone(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let w_commit = write_commit_object(repo, &w_commit_data, None)?;

    Ok(Some(BuiltStash {
        w_commit,
        w_tree,
        message: w_message.trim_end().to_owned(),
        untracked_paths,
    }))
}

/// Create a Git-compatible stash commit without updating `refs/stash`.
///
/// Returns `None` when the index and worktree match `HEAD` and there are no
/// untracked paths to save (unless `include_untracked` finds untracked files).
///
/// # Errors
///
/// Returns [`Error::StashNoInitialCommit`] when `HEAD` does not resolve to a commit,
/// or propagates index, diff, and object-database failures.
pub fn create_stash(repo: &Repository, options: &StashCreateOptions) -> Result<Option<ObjectId>> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::PathError("cannot stash in a bare repository".into()))?;
    Ok(build_stash_commit(repo, work_tree, options)?.map(|b| b.w_commit))
}

/// Point `refs/stash` at `oid` and append a reflog entry with `message`.
///
/// # Errors
///
/// Propagates ref and reflog update failures.
pub fn store_stash(repo: &Repository, oid: ObjectId, message: &str, identity: &str) -> Result<()> {
    let old_oid = refs::resolve_ref(&repo.git_dir, "refs/stash").unwrap_or(ObjectId::zero());
    refs::write_ref(&repo.git_dir, "refs/stash", &oid)?;
    append_reflog(
        &repo.git_dir,
        "refs/stash",
        &old_oid,
        &oid,
        identity,
        message,
        true,
    )
}

/// Create a stash entry, store it, and reset the worktree and index to `HEAD`.
///
/// Returns `None` when there is nothing to stash. Untracked paths included in
/// the stash are removed from the worktree after the reset.
///
/// # Errors
///
/// Same as [`create_stash`] and [`store_stash`], plus checkout failures.
pub fn push_stash(repo: &Repository, options: &StashCreateOptions) -> Result<Option<ObjectId>> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::PathError("cannot stash in a bare repository".into()))?;
    let Some(built) = build_stash_commit(repo, work_tree, options)? else {
        return Ok(None);
    };
    store_stash(repo, built.w_commit, &built.message, &options.identity)?;
    let head = resolve_head(&repo.git_dir)?;
    let head_oid = head_commit_oid(&head)?;
    let head_tree = {
        let obj = repo.odb.read(&head_oid)?;
        parse_commit(&obj.data)?.tree
    };
    checkout_between_trees(repo, Some(&built.w_tree), &head_tree)?;
    let mut head_index = index_from_tree(repo, &head_tree)?;
    repo.write_index(&mut head_index)?;
    if !built.untracked_paths.is_empty() {
        remove_stashed_untracked(repo, work_tree, &built.untracked_paths)?;
    }
    Ok(Some(built.w_commit))
}

/// List stash entries from `refs/stash` reflog (newest first).
///
/// # Errors
///
/// Propagates reflog read failures.
pub fn list_stashes(repo: &Repository) -> Result<Vec<StashEntry>> {
    let entries = read_reflog(&repo.git_dir, "refs/stash")?;
    Ok(entries
        .into_iter()
        .rev()
        .enumerate()
        .map(|(i, e)| StashEntry {
            index: i,
            oid: e.new_oid,
            message: e.message,
        })
        .collect())
}

fn stash_oid_at(repo: &Repository, n: usize) -> Result<ObjectId> {
    let entries = list_stashes(repo)?;
    entries
        .into_iter()
        .find(|e| e.index == n)
        .map(|e| e.oid)
        .ok_or(Error::StashNotFound { n })
}

/// Remove `stash@{{n}}` from the reflog and move `refs/stash` to the next entry.
///
/// Deletes `refs/stash` when the reflog becomes empty.
///
/// # Errors
///
/// Returns [`Error::StashNotFound`] when `n` is out of range.
pub fn drop_stash(repo: &Repository, n: usize, identity: &str) -> Result<()> {
    let entries = read_reflog(&repo.git_dir, "refs/stash")?;
    if n >= entries.len() {
        return Err(Error::StashNotFound { n });
    }
    let _ = identity;
    delete_reflog_entries_rechain(&repo.git_dir, "refs/stash", &[n])?;
    let remaining = read_reflog(&repo.git_dir, "refs/stash")?;
    if let Some(top_entry) = remaining.last() {
        refs::write_ref(&repo.git_dir, "refs/stash", &top_entry.new_oid)?;
    } else {
        refs::delete_ref(&repo.git_dir, "refs/stash")?;
    }
    Ok(())
}

/// Apply `stash@{{n}}` and drop it when there are no conflicts.
///
/// Returns `true` when merge conflicts occurred (the stash entry is kept).
///
/// # Errors
///
/// Propagates [`apply_stash`] and [`drop_stash`] failures.
pub fn pop_stash(
    repo: &Repository,
    work_tree: &Path,
    n: usize,
    restore_index: bool,
    identity: &str,
) -> Result<bool> {
    let oid = stash_oid_at(repo, n)?;
    let conflicts = apply_stash(repo, work_tree, &oid, restore_index, identity)?;
    if !conflicts {
        drop_stash(repo, n, identity)?;
    }
    Ok(conflicts)
}

/// Tree diff of stash W against its `HEAD` parent (`stash show` without `--index`).
///
/// # Errors
///
/// Returns [`Error::StashNotFound`] or [`Error::CorruptStash`] when the entry is invalid.
pub fn stash_diff(repo: &Repository, n: usize) -> Result<Vec<DiffEntry>> {
    let oid = stash_oid_at(repo, n)?;
    let obj = repo.odb.read(&oid)?;
    let stash_commit = parse_commit(&obj.data)?;
    let head_parent = stash_commit
        .parents
        .first()
        .ok_or(Error::CorruptStash("expected at least 2 parents"))?;
    let head_obj = repo.odb.read(head_parent)?;
    let head_commit = parse_commit(&head_obj.data)?;
    diff_trees(
        &repo.odb,
        Some(&head_commit.tree),
        Some(&stash_commit.tree),
        "",
    )
}

/// Paths whose worktree content the stash would change vs. its HEAD-at-stash base.
pub fn stash_worktree_change_paths(
    repo: &Repository,
    stash_commit: &CommitData,
) -> Result<BTreeSet<String>> {
    let head_at_stash = stash_commit
        .parents
        .first()
        .ok_or(Error::CorruptStash("expected at least 2 parents"))?;
    let stash_tree_entries = flatten_tree_full(&repo.odb, &stash_commit.tree, "")?;
    let head_obj = repo.odb.read(head_at_stash)?;
    let head_commit = parse_commit(&head_obj.data)?;
    let base_tree_entries = flatten_tree_full(&repo.odb, &head_commit.tree, "")?;

    let changes = collect_flat_tree_worktree_changes(&base_tree_entries, &stash_tree_entries);
    Ok(changes.into_iter().map(|(path, _)| path).collect())
}

/// Refuse to apply when the stash would clobber a locally-modified file.
pub fn check_stash_apply_would_overwrite_local_changes(
    repo: &Repository,
    work_tree: &Path,
    stash_commit: &CommitData,
) -> Result<()> {
    let current_index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };

    for path in stash_worktree_change_paths(repo, stash_commit)? {
        let file_path = work_tree.join(&path);
        let Some(idx_entry) = current_index.get(path.as_bytes(), 0) else {
            continue;
        };
        if idx_entry.mode == MODE_GITLINK {
            continue;
        }
        match worktree_bytes_for_index_mode(&file_path, idx_entry.mode) {
            Ok(contents) => {
                if let Ok(idx_blob) = repo.odb.read(&idx_entry.oid) {
                    if contents != idx_blob.data {
                        return Err(Error::StashWouldOverwriteLocalChanges {
                            paths: path.clone(),
                        });
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Apply a stash commit onto the current worktree and index.
///
/// Returns `true` if there were conflicts. Mirrors `git stash apply`:
/// three-way-merges changed files when HEAD has moved since the stash was
/// created, restores the index from the stash index parent when `restore_index`
/// is set (otherwise tracks current HEAD at touched paths), and materializes
/// any untracked-files parent. The CLI handles the `Dropped …`/conflict-kept
/// messaging around this.
pub fn apply_stash(
    repo: &Repository,
    work_tree: &Path,
    stash_oid: &ObjectId,
    restore_index: bool,
    _identity: &str,
) -> Result<bool> {
    let obj = repo.odb.read(stash_oid)?;
    let stash_commit = parse_commit(&obj.data)?;

    if stash_commit.parents.len() < 2 {
        return Err(Error::CorruptStash("expected at least 2 parents"));
    }

    check_stash_apply_would_overwrite_local_changes(repo, work_tree, &stash_commit)?;

    let head_at_stash = &stash_commit.parents[0];
    let index_commit_oid = &stash_commit.parents[1];

    // Load current index
    let current_index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };

    // Read stash trees
    let stash_tree_entries = flatten_tree_full(&repo.odb, &stash_commit.tree, "")?;

    // Read HEAD-at-stash tree (base)
    let head_at_stash_obj = repo.odb.read(head_at_stash)?;
    let head_at_stash_commit = parse_commit(&head_at_stash_obj.data)?;
    let base_tree_entries = flatten_tree_full(&repo.odb, &head_at_stash_commit.tree, "")?;

    let wt_changes = collect_flat_tree_worktree_changes(&base_tree_entries, &stash_tree_entries);

    // Check for conflicts: does the worktree have local modifications to files
    // that the stash also wants to change?
    for (path, _) in &wt_changes {
        let file_path = work_tree.join(path);
        // Get the current index entry for this file
        if let Some(idx_entry) = current_index.get(path.as_bytes(), 0) {
            if idx_entry.mode == MODE_GITLINK {
                // Submodule: comparing index blob in the superproject ODB is wrong; t7402 expects
                // stash apply to succeed while the nested repo keeps its own HEAD.
                continue;
            }
            // Read the worktree file
            match worktree_bytes_for_index_mode(&file_path, idx_entry.mode) {
                Ok(contents) => {
                    if let Ok(idx_blob) = repo.odb.read(&idx_entry.oid) {
                        if contents != idx_blob.data {
                            return Err(Error::StashWouldOverwriteLocalChanges {
                                paths: path.clone(),
                            });
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File doesn't exist in worktree — could be deleted locally
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    // Read index commit tree
    let idx_obj = repo.odb.read(index_commit_oid)?;
    let idx_commit = parse_commit(&idx_obj.data)?;
    let idx_tree_entries = flatten_tree_full(&repo.odb, &idx_commit.tree, "")?;
    // Determine if HEAD has moved since the stash was created
    let current_head = resolve_head(&repo.git_dir)?;
    let current_head_oid = current_head.oid().copied();
    let head_moved = current_head_oid.as_ref() != Some(head_at_stash);

    // Current HEAD tree (three-way merge when HEAD moved; index reset without `--index`).
    let current_head_loaded: Vec<FlatTreeEntry>;
    let current_head_entries: &[FlatTreeEntry] = if head_moved {
        let Some(head_oid) = current_head_oid.as_ref() else {
            return Err(Error::StashMissingHead);
        };
        let head_obj = repo.odb.read(head_oid)?;
        let head_commit = parse_commit(&head_obj.data)?;
        current_head_loaded = flatten_tree_full(&repo.odb, &head_commit.tree, "")?;
        &current_head_loaded
    } else {
        &base_tree_entries
    };

    let mut has_conflicts = false;
    let mut new_index = current_index.clone();

    // Pre-check: detect type conflicts where the stash wants to place a FILE
    // at a path that is currently a DIRECTORY in the worktree, or vice-versa.
    // We must check BEFORE removing anything (deletions below may clear dirs).
    for (path, change) in &wt_changes {
        if let Some(entry) = change {
            if entry.mode == MODE_GITLINK {
                continue;
            }
            let file_path = work_tree.join(path);
            if file_path.is_dir() {
                // A file from the stash conflicts with a directory in the worktree.
                // Mark as conflicted and remove the directory so we can write the file.
                has_conflicts = true;
                let _ = fs::remove_dir_all(&file_path);
            }
        }
    }

    // First pass: process deletions (None entries) before additions to avoid
    // type conflicts (e.g., trying to write a file where a directory exists).
    for (path, change) in &wt_changes {
        if change.is_some() {
            continue;
        }
        let file_path = work_tree.join(path);
        if file_path.is_dir() {
            let git_meta = file_path.join(".git");
            if git_meta.is_file() || git_meta.is_dir() {
                continue;
            }
            let _ = fs::remove_dir_all(&file_path);
        } else {
            let _ = fs::remove_file(&file_path);
        }
        if let Some(parent) = file_path.parent() {
            remove_empty_dirs(parent, work_tree, repo.environment());
        }
    }

    // Apply working tree changes (with three-way merge when HEAD has moved)
    for (path, change) in &wt_changes {
        let file_path = work_tree.join(path);
        match change {
            Some(entry) => {
                if let Some(parent) = file_path.parent() {
                    // If a component of the parent is a file, remove it first
                    let mut cur = work_tree.to_path_buf();
                    if let Ok(rel) = file_path
                        .parent()
                        .unwrap_or(work_tree)
                        .strip_prefix(work_tree)
                    {
                        for comp in rel.components() {
                            cur.push(comp);
                            if cur.exists() && !cur.is_dir() {
                                let _ = fs::remove_file(&cur);
                            }
                        }
                    }
                    fs::create_dir_all(parent)?;
                }
                if entry.mode == MODE_GITLINK {
                    if file_path.is_file() || file_path.is_symlink() {
                        let _ = fs::remove_file(&file_path);
                    } else if file_path.is_dir() {
                        let git_meta = file_path.join(".git");
                        if !(git_meta.is_file() || git_meta.is_dir()) {
                            fs::remove_dir_all(&file_path)?;
                        }
                    }
                    fs::create_dir_all(&file_path)?;
                    continue;
                }

                let stash_blob = repo.odb.read(&entry.oid)?;

                if entry.mode == MODE_SYMLINK {
                    let target = String::from_utf8(stash_blob.data)
                        .map_err(|_| Error::StashSymlinkNotUtf8)?;
                    if file_path.exists() || file_path.symlink_metadata().is_ok() {
                        let _ = fs::remove_file(&file_path);
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&target, &file_path)?;
                    // Windows lacks unprivileged symlinks; keep the worktree
                    // populated by writing the target as a regular file.
                    #[cfg(not(unix))]
                    fs::write(&file_path, target.as_bytes())?;
                } else if head_moved {
                    // Three-way merge: base (head_at_stash), ours (current HEAD), theirs (stash)
                    let base_content = flat_tree_lookup(&base_tree_entries, path)
                        .and_then(|e| repo.odb.read(&e.oid).ok())
                        .map(|o| o.data)
                        .unwrap_or_default();
                    let ours_content = flat_tree_lookup(current_head_entries, path)
                        .and_then(|e| repo.odb.read(&e.oid).ok())
                        .map(|o| o.data)
                        .unwrap_or_default();
                    let theirs_content = stash_blob.data;

                    // If ours == base, no conflict (only stash changed this file)
                    if ours_content == base_content {
                        write_regular_file_replacing_symlink(&file_path, &theirs_content)?;
                    } else if ours_content == theirs_content {
                        // Both changed the same way, no conflict
                        write_regular_file_replacing_symlink(&file_path, &ours_content)?;
                    } else {
                        // Both sides changed differently — try content merge
                        use crate::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
                        let input = MergeInput {
                            base: &base_content,
                            ours: &ours_content,
                            theirs: &theirs_content,
                            label_ours: "Updated upstream",
                            label_base: "Stashed changes",
                            label_theirs: "Stashed changes",
                            favor: MergeFavor::None,
                            style: ConflictStyle::Merge,
                            marker_size: 7,
                            diff_algorithm: None,
                            ignore_all_space: false,
                            ignore_space_change: false,
                            ignore_space_at_eol: false,
                            ignore_cr_at_eol: false,
                        };
                        let output = merge(&input)?;
                        write_regular_file_replacing_symlink(&file_path, &output.content)?;
                        if output.conflicts > 0 {
                            has_conflicts = true;
                            // Write conflict stages to index
                            let path_bytes = path.as_bytes();
                            // Remove existing stage-0 entry
                            new_index
                                .entries
                                .retain(|e| e.path != path_bytes || e.stage() != 0);
                            // Adding non-zero stages drops this path from any valid stage-0
                            // cache-tree; invalidate it (Git's add_index_entry ->
                            // cache_tree_invalidate_path) so a stale TREE extension is not written
                            // alongside the conflicted index (otherwise GIT_TEST_CHECK_CACHE_TREE
                            // rejects it with "corrupted cache-tree has entries not present in
                            // index"; t7600 'merge with conflicted --autostash changes').
                            new_index.invalidate_cache_tree_for_path(path_bytes);
                            // Add stage entries
                            if let Some(base_entry) = flat_tree_lookup(&base_tree_entries, path) {
                                add_stage_entry(
                                    &mut new_index,
                                    path_bytes,
                                    &base_entry.oid,
                                    base_entry.mode,
                                    1,
                                );
                            }
                            if let Some(ours_entry) = flat_tree_lookup(current_head_entries, path) {
                                let mode = current_index
                                    .get(path_bytes, 0)
                                    .map(|e| e.mode)
                                    .unwrap_or(0o100644);
                                add_stage_entry(
                                    &mut new_index,
                                    path_bytes,
                                    &ours_entry.oid,
                                    mode,
                                    2,
                                );
                            }
                            add_stage_entry(&mut new_index, path_bytes, &entry.oid, entry.mode, 3);
                        }
                    }
                } else {
                    write_regular_file_replacing_symlink(&file_path, &stash_blob.data)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if entry.mode == MODE_EXECUTABLE {
                            let perms = std::fs::Permissions::from_mode(0o755);
                            fs::set_permissions(&file_path, perms)?;
                        }
                    }
                }
            }
            None => {
                // Deleted in stash
                let _ = fs::remove_file(&file_path);
                if let Some(parent) = file_path.parent() {
                    remove_empty_dirs(parent, work_tree, repo.environment());
                }
            }
        }
    }

    // Update the index

    if restore_index {
        // --index: restore the index to the stash's index state for changed files
        for idx_entry in &idx_tree_entries {
            let path = &idx_entry.path;
            let base_oid = flat_tree_lookup(&base_tree_entries, path).map(|e| &e.oid);
            if base_oid != Some(&idx_entry.oid) {
                // This file was staged differently from base in the stash
                let path_bytes = path.as_bytes();
                if let Some(ie) = new_index.get_mut(path_bytes, 0) {
                    ie.oid = idx_entry.oid;
                    ie.mode = idx_entry.mode;
                } else {
                    let flags = if path.len() > 0xFFF {
                        0xFFF
                    } else {
                        path.len() as u16
                    };
                    new_index.push_entry_unsorted(IndexEntry {
                        ctime_sec: 0,
                        ctime_nsec: 0,
                        mtime_sec: 0,
                        mtime_nsec: 0,
                        dev: 0,
                        ino: 0,
                        mode: idx_entry.mode,
                        uid: 0,
                        gid: 0,
                        size: 0,
                        oid: idx_entry.oid,
                        flags,
                        flags_extended: None,
                        path: path_bytes.to_vec(),
                        base_index_pos: 0,
                    });
                }
            }
        }
        // Handle files added in the index but not in base
        // (already covered above)
        for (path, _) in &wt_changes {
            if let Some(ie) = new_index.get_mut(path.as_bytes(), 0) {
                ie.set_skip_worktree(false);
            }
        }
        new_index.sort();
    } else {
        // Without --index: index tracks current HEAD for paths the stash touched
        // (worktree gets the stashed changes; index matches HEAD at those paths).
        //
        // Exception: paths that exist in the stash index parent but not on **current** HEAD
        // (e.g. a newly `git add`ed file) must be re-staged from the stash index parent
        // (t3903 `stash an added file`).
        let mut touched: BTreeSet<String> = wt_changes.iter().map(|(p, _)| p.clone()).collect();
        for entry in &idx_tree_entries {
            if flat_tree_lookup(&base_tree_entries, &entry.path).is_none() {
                touched.insert(entry.path.clone());
            }
        }
        for path in &touched {
            if let Some(te) = flat_tree_lookup(current_head_entries, path) {
                let path_bytes = path.as_bytes();
                let size = if te.mode == MODE_SYMLINK || te.mode == MODE_GITLINK {
                    0u32
                } else {
                    repo.odb.read(&te.oid)?.data.len() as u32
                };
                let new_entry = IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: te.mode,
                    uid: 0,
                    gid: 0,
                    size,
                    oid: te.oid,
                    flags: path_bytes.len().min(0xFFF) as u16,
                    flags_extended: None,
                    path: path_bytes.to_vec(),
                    base_index_pos: 0,
                };
                // Do not replace unmerged index entries: `stage_file` strips stages 1–3, which
                // would hide merge conflicts after stash apply (t9903 conflict prompt).
                let has_unmerged = new_index
                    .entries
                    .iter()
                    .any(|e| e.path == path_bytes && e.stage() > 0);
                if !has_unmerged {
                    new_index.stage_file(new_entry);
                }
            } else {
                let path_bytes = path.as_bytes();
                let has_unmerged = new_index
                    .entries
                    .iter()
                    .any(|e| e.path == path_bytes && e.stage() > 0);
                if has_unmerged {
                    continue;
                }
                if let Some(ie) = flat_tree_lookup(&idx_tree_entries, path) {
                    let had_staged = match flat_tree_lookup(&base_tree_entries, path) {
                        Some(b) => b.oid != ie.oid || b.mode != ie.mode,
                        None => true,
                    };
                    if had_staged {
                        let size = if ie.mode == MODE_SYMLINK || ie.mode == MODE_GITLINK {
                            0u32
                        } else {
                            repo.odb.read(&ie.oid)?.data.len() as u32
                        };
                        new_index.stage_file(IndexEntry {
                            ctime_sec: 0,
                            ctime_nsec: 0,
                            mtime_sec: 0,
                            mtime_nsec: 0,
                            dev: 0,
                            ino: 0,
                            mode: ie.mode,
                            uid: 0,
                            gid: 0,
                            size,
                            oid: ie.oid,
                            flags: path_bytes.len().min(0xFFF) as u16,
                            flags_extended: None,
                            path: path_bytes.to_vec(),
                            base_index_pos: 0,
                        });
                    } else {
                        new_index.remove(path_bytes);
                    }
                } else {
                    new_index.remove(path_bytes);
                }
            }
        }
        new_index.sort();
    }

    if has_conflicts {
        new_index.sort();
        // A conflicted index (unmerged stages) cannot have a valid stage-0 cache-tree; drop the
        // TREE extension so write_index does not persist a stale one (which would fail
        // GIT_TEST_CHECK_CACHE_TREE verification with "corrupted cache-tree has entries not
        // present in index"). Mirrors Git, which only keeps a cache-tree for a fully merged index.
        new_index.clear_cache_tree();
    }
    // Refresh cached stat for entries restored from the stash trees whose worktree content matches
    // the recorded OID, so a following `git diff-files` reflects only genuine differences (t3903
    // 'stash apply --index refreshes the index').
    if !has_conflicts {
        let rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &new_index).ok();
        let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
        let _ = crate::diff::refresh_index_stat_content_verified_with_rules(
            &repo.odb,
            &repo.git_dir,
            &mut new_index,
            work_tree,
            None,
            repo.config().ok().as_deref(),
            None,
            rules_arc.as_ref(),
        )?;
    }
    repo.write_index(&mut new_index)
        .map_err(|e| Error::StashIndexWrite(e.to_string()))?;

    // Apply untracked files if present (3rd parent)
    if stash_commit.parents.len() >= 3 {
        let ut_oid = &stash_commit.parents[2];
        let ut_obj = repo.odb.read(ut_oid)?;
        let ut_commit = parse_commit(&ut_obj.data)?;
        let ut_entries = flatten_tree_full(&repo.odb, &ut_commit.tree, "")?;
        for entry in &ut_entries {
            let file_path = work_tree.join(&entry.path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let blob = repo.odb.read(&entry.oid)?;
            fs::write(&file_path, &blob.data)?;
        }
    }

    Ok(has_conflicts)
}

const STASH_REF: &str = "refs/stash";

/// Remove one stash commit from `refs/stash` when it is the reflog tip.
///
/// # Returns
///
/// `true` when the stash entry was removed.
///
/// # Errors
///
/// Propagates ref and reflog update failures.
pub fn drop_stash_commit(git_dir: &Path, stash_oid: &ObjectId) -> Result<bool> {
    let entries = read_reflog(git_dir, STASH_REF)?;
    if entries.is_empty() {
        return Ok(false);
    }
    let Some(tip) = entries.last() else {
        return Ok(false);
    };
    if tip.new_oid != *stash_oid {
        return Ok(false);
    }
    if entries.len() == 1 {
        truncate_last_reflog_line(git_dir, STASH_REF)?;
        delete_ref(git_dir, STASH_REF)?;
    } else {
        let prev = entries[entries.len() - 2].new_oid;
        truncate_last_reflog_line(git_dir, STASH_REF)?;
        write_ref(git_dir, STASH_REF, &prev)?;
    }
    Ok(true)
}

/// Apply and drop the merge autostash recorded in `MERGE_AUTOSTASH`, matching `git commit` after
/// `git merge --autostash`.
///
/// # Returns
///
/// `true` when applying the autostash left index/worktree conflicts (the stash entry is kept).
///
/// # Errors
///
/// Propagates stash apply, index, and I/O failures.
pub fn apply_merge_autostash(repo: &Repository) -> Result<bool> {
    let path = repo.git_dir.join("MERGE_AUTOSTASH");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(false);
    }
    let stash_oid = ObjectId::from_hex(trimmed)?;
    let work_tree = repo.work_tree.as_deref().ok_or_else(|| {
        Error::Message("cannot apply merge autostash in a bare repository".into())
    })?;
    let conflicts = apply_stash(repo, work_tree, &stash_oid, false, "")?;
    let _ = fs::remove_file(&path);
    if !conflicts {
        let _ = drop_stash_commit(&repo.git_dir, &stash_oid)?;
    }
    Ok(conflicts)
}

#[cfg(test)]
mod coverage_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::index::{entry_from_stat, MODE_REGULAR};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    fn init_repo(root: &Path) -> Repository {
        let git = root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\tlogAllRefUpdates = true\n",
        )
        .unwrap();
        let repo = Repository::open(&git, Some(root)).unwrap();
        let wt = root.join("f");
        fs::write(&wt, b"base\n").unwrap();
        let mut index = repo.load_index().unwrap();
        let oid = repo.odb.write(ObjectKind::Blob, b"base\n").unwrap();
        index.add_or_replace(entry_from_stat(&wt, b"f", oid, MODE_REGULAR).unwrap());
        index.sort();
        repo.write_index(&mut index).unwrap();
        let tree = write_stash_tree_from_index(&repo, &mut index).unwrap();
        let parent = refs::resolve_ref(&repo.git_dir, "HEAD").ok();
        let commit = CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "init\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let commit_oid = write_commit_object(&repo, &commit, None).unwrap();
        refs::write_ref(&repo.git_dir, "HEAD", &commit_oid).unwrap();
        refs::write_ref(&repo.git_dir, "refs/heads/main", &commit_oid).unwrap();
        repo
    }

    #[test]
    fn create_store_list_drop_and_diff_public_api() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let ident = "T <t@t> 1 +0000";
        fs::write(tmp.path().join("f"), b"changed\n").unwrap();
        let opts = StashCreateOptions {
            message: Some("msg".into()),
            include_untracked: false,
            identity: ident.into(),
        };
        let oid = create_stash(&repo, &opts).expect("create").expect("some");
        store_stash(&repo, oid, "On main: msg", ident).expect("store");
        let listed = list_stashes(&repo).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].oid, oid);
        let diffs = stash_diff(&repo, 0).expect("diff");
        assert!(!diffs.is_empty());
        drop_stash(&repo, 0, ident).expect("drop");
        assert!(list_stashes(&repo).expect("list").is_empty());
    }

    #[test]
    fn push_stash_returns_none_when_clean() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let opts = StashCreateOptions {
            message: None,
            include_untracked: false,
            identity: "T <t@t> 1 +0000".into(),
        };
        assert!(push_stash(&repo, &opts).expect("push").is_none());
    }
}
