//! Stash create/store/list/drop/apply/pop/show primitives.
//!
//! The library builds Git-compatible stash commits (W/I/U parents), updates
//! `refs/stash` and its reflog, and applies entries onto the worktree and index.
//! The `grit` binary keeps argument parsing, human/`--json`/`--markdown`
//! output, and exit-code mapping. [`apply_stash_entry`] and [`pop_stash_entry`]
//! report the paths left conflicted (a pop keeps the entry when there are any);
//! [`apply_stash`] and [`pop_stash`] return only whether conflicts occurred.
//!
//! Flattening and worktree helpers ([`FlatTreeEntry`], [`flatten_tree_full`],
//! [`add_stage_entry`], [`worktree_bytes_for_index_mode`],
//! [`write_regular_file_replacing_symlink`], [`remove_empty_dirs`]) are shared
//! building blocks for apply and for embedders that manipulate stash trees.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, format_mode,
    worktree_differs_from_index_entry, zero_oid, DiffEntry, DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, Index, IndexEntry, MODE_GITLINK, MODE_SYMLINK, MODE_TREE};
// `MODE_EXECUTABLE` only drives the Unix executable-bit application below.
use crate::apply::{apply_patch_to_index, FilePatch, IndexApplyOptions};
use crate::check_ref_format::validate_branch_short_name;
use crate::diff::hunk_id::DiffSide;
#[cfg(unix)]
use crate::index::MODE_EXECUTABLE;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_tree, CommitData, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pathspec::{
    has_glob_chars, matches_pathspec_list, pathspec_is_exclude, PathspecListMatcher,
};
use crate::porcelain::branch_error::BranchError;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::checkout::smudge_bytes_for_worktree_path;
use crate::porcelain::clean::{is_nested_git_worktree, path_in_nested_git_worktree};
use crate::porcelain::commit::write_commit_object;
use crate::porcelain::hunk_select::{
    diff_options_for_hunk_select, resolve_selection, reverse_apply_patches_to_worktree,
    HunkSelection, WorktreeReverseApplyOptions,
};
use crate::porcelain::index_from_tree::{index_from_tree, index_from_tree_with_worktree_stats};
use crate::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use crate::porcelain::status::{
    collect_untracked_and_ignored_inner, expand_untracked_for_staging_with_rules, IgnoredMode,
    UntrackedCacheMissAction, UntrackedScan,
};
use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use crate::porcelain::worktree_guard::untracked_would_be_overwritten_by_tree_changes;
use crate::reflog::{delete_reflog_entries_rechain_on_store, read_reflog_on_store};
use crate::refs::{append_reflog_on_store, delete_ref_on_store, update_direct_ref_on_store};
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};
use crate::worktree_rules::WorktreeRules;
use crate::write_tree::{is_empty_tree_oid, write_tree_update_index, WriteTreeFlags};

/// A single blob entry from a recursively flattened tree.
#[derive(Clone)]
pub struct FlatTreeEntry {
    /// Repository-relative path using `/` separators.
    pub path: String,
    /// Git tree entry mode (file, symlink, gitlink, …).
    pub mode: u32,
    /// Blob or subtree object id for this path.
    pub oid: ObjectId,
}

/// Recursively flatten a tree into (path, mode, oid) entries.
pub fn flatten_tree_full(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    protect: crate::path_validate::ProtectOptions,
) -> Result<Vec<FlatTreeEntry>> {
    let obj = odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();
    for entry in entries {
        let entry_name = String::from_utf8_lossy(&entry.name).to_string();
        let full_path = if prefix.is_empty() {
            entry_name.clone()
        } else {
            format!("{prefix}/{entry_name}")
        };
        crate::path_population::validate_tree_walk_entry(
            prefix.as_bytes(),
            &entry.name,
            entry.mode,
            protect,
        )?;
        if entry.mode == 0o40000 {
            let sub = flatten_tree_full(odb, &entry.oid, &full_path, protect)?;
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

pub(crate) fn flatten_tree_full_for_repo(
    repo: &Repository,
    tree_oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<FlatTreeEntry>> {
    let protect = crate::path_validate::ProtectOptions::from_config(&*repo.config()?);
    flatten_tree_full(repo.odb(), tree_oid, prefix, protect)
}

/// Lookup a path in a lexicographically sorted [`flatten_tree_full`] slice.
pub fn flat_tree_lookup<'a>(entries: &'a [FlatTreeEntry], path: &str) -> Option<&'a FlatTreeEntry> {
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
    crate::diff::read_worktree_bytes_for_mode(path, mode)
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

/// Which local changes [`push_stash`] saves and how the worktree/index are left afterward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StashMode {
    /// Stash staged and unstaged tracked changes (default).
    #[default]
    All,
    /// Stash only index changes vs `HEAD`; unstaged worktree edits on other paths remain.
    StagedOnly,
    /// Stash everything, then reset the worktree to match the index (staged content stays).
    KeepIndex,
}

/// Paths that would be recorded by [`preview_stash`] or a `--dry-run` push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashPreview {
    /// Repository-relative paths with tracked changes that would be stashed.
    pub paths: Vec<String>,
    /// Untracked paths that would be included when `include_untracked` is set.
    pub untracked: Vec<String>,
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
    /// When set, stash only these unstaged hunks (index is left unchanged on push).
    pub selection: Option<HunkSelection>,
    /// Pathspecs narrowing hunk resolution (same as `grit add --hunks`).
    pub pathspecs: Vec<String>,
    /// When true, callers must use [`preview_stash`] instead of [`push_stash`].
    pub dry_run: bool,
    /// Push mode (`--staged`, `--keep-index`, or default).
    pub mode: StashMode,
}

impl StashCreateOptions {
    /// Reject flag combinations Git and grit do not support.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StashOptionConflict`] when options conflict.
    pub fn validate(&self) -> Result<()> {
        if self.mode == StashMode::StagedOnly && self.include_untracked {
            return Err(Error::StashOptionConflict(
                "cannot combine --staged with --include-untracked".into(),
            ));
        }
        if self.mode == StashMode::StagedOnly && selection_is_active(self) {
            return Err(Error::StashOptionConflict(
                "cannot combine --staged with --hunks or --lines".into(),
            ));
        }
        if self.mode == StashMode::KeepIndex && selection_is_active(self) {
            return Err(Error::StashOptionConflict(
                "cannot combine --keep-index with --hunks or --lines".into(),
            ));
        }
        Ok(())
    }

    /// Staged-only push cannot run when a staged path also has unstaged worktree edits.
    pub fn validate_staged_worktree(
        &self,
        repo: &Repository,
        work_tree: &Path,
        head_tree: &ObjectId,
    ) -> Result<()> {
        if self.mode != StashMode::StagedOnly {
            return Ok(());
        }
        let mut index = repo.load_index()?;
        let staged: HashSet<String> =
            staged_change_paths_for_options(repo, &index, head_tree, self)?
                .into_iter()
                .collect();
        if staged.is_empty() {
            return Ok(());
        }
        let index_mtime = index_file_mtime(&repo.index_path());
        let worktree_rules = WorktreeRules::from_repository(repo, &index)?;
        let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
        let diff_opts = DiffIndexToWorktreeOptions {
            index_mtime,
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: repo.config().ok(),
            worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
            ..Default::default()
        };
        let (unstaged, _) =
            diff_index_to_worktree_with_options(repo.odb(), &mut index, work_tree, diff_opts)?;
        for entry in unstaged {
            let path = entry.path();
            if staged.contains(path) {
                return Err(Error::StashOptionConflict(format!(
                    "cannot stash staged changes for '{path}': worktree has unstaged changes"
                )));
            }
        }
        Ok(())
    }
}

/// Options for [`apply_stash_entry`] and [`pop_stash_entry`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StashApplyOptions {
    /// Restore the stashed index parent instead of keeping the current index at touched paths.
    pub restore_index: bool,
}

/// Options for [`stash_diff`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StashDiffOptions {
    /// Include untracked files from the stash U parent (same as `git stash show -u`).
    pub include_untracked: bool,
}

/// Result of [`stash_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashBranchOutcome {
    /// Short name of the branch created at the stash base commit.
    pub branch: String,
    /// Stash base commit (parent 1 of the W commit).
    pub base: ObjectId,
    /// Stash index that was branched.
    pub index: usize,
    /// Whether applying the stash left merge conflicts.
    pub conflicts: bool,
    /// Paths with unmerged index entries when [`Self::conflicts`] is true.
    pub conflict_paths: Vec<String>,
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
    let commit = repo.odb().read_commit(head_oid)?;
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

fn staged_change_paths(
    repo: &Repository,
    index: &Index,
    head_tree: &ObjectId,
) -> Result<Vec<String>> {
    let staged = diff_index_to_tree(repo.odb(), index, Some(head_tree), true)?;
    Ok(staged.into_iter().map(|e| e.path().to_string()).collect())
}

fn filter_paths_by_pathspecs(paths: Vec<String>, pathspecs: &[String]) -> Vec<String> {
    if pathspecs.is_empty() {
        return paths;
    }
    paths
        .into_iter()
        .filter(|path| matches_pathspec_list(path, pathspecs))
        .collect()
}

fn staged_change_paths_for_options(
    repo: &Repository,
    index: &Index,
    head_tree: &ObjectId,
    options: &StashCreateOptions,
) -> Result<Vec<String>> {
    let paths = staged_change_paths(repo, index, head_tree)?;
    Ok(filter_paths_by_pathspecs(paths, &options.pathspecs))
}

fn has_stashable_changes(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    options: &StashCreateOptions,
) -> Result<bool> {
    let mut index = repo.load_index()?;
    if options.mode == StashMode::StagedOnly {
        return Ok(!staged_change_paths_for_options(repo, &index, head_tree, options)?.is_empty());
    }
    if !diff_index_to_tree(repo.odb(), &index, Some(head_tree), true)?.is_empty() {
        return Ok(true);
    }
    let include_untracked = options.include_untracked;
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: true,
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        config: repo.config().ok(),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(repo.odb(), &mut index, work_tree, diff_opts)?;
    if !unstaged.is_empty() {
        return Ok(true);
    }
    if !include_untracked {
        return Ok(false);
    }
    let worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let (untracked, _, _) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::No,
            show_all: false,
            sort_paths: true,
            use_untracked_cache: false,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
        },
        &worktree_rules,
    )?;
    Ok(!untracked.is_empty())
}

fn collect_untracked_paths(repo: &Repository, work_tree: &Path) -> Result<Vec<String>> {
    let mut index = repo.load_index()?;
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let (untracked, _, _) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::No,
            show_all: true,
            sort_paths: true,
            use_untracked_cache: false,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
        },
        &worktree_rules,
    )?;
    let expanded = expand_untracked_for_staging_with_rules(
        repo,
        &mut index,
        work_tree,
        untracked,
        &[],
        &worktree_rules,
    )?;
    Ok(expanded
        .into_iter()
        .filter(|path| !path_in_nested_git_worktree(work_tree, path))
        .collect())
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
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        config: repo.config().ok(),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(repo.odb(), index, work_tree, diff_opts)?;
    let mut worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let precompose = crate::precompose_config::effective_core_precomposeunicode_with_config(
        Some(repo.git_dir()),
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
    let staged_mode = crate::index::WorktreeModePolicy::from_config(rules.config())
        .index_mode_for_worktree(mode_from_metadata(&meta), Some(ie.mode));
    let file_attrs = rules.file_attrs(rel_path, false);
    let refresh = classify_worktree_entry_for_add(&WorktreeAddRefreshParams {
        odb: repo.odb(),
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
    let recorded_mode = index.get(index_relpath.as_bytes(), 0).map(|e| e.mode);
    let mode = crate::index::WorktreeModePolicy::from_config(rules.config())
        .index_mode_for_worktree(mode_from_metadata(&meta), recorded_mode);
    if mode == MODE_TREE {
        return Ok(());
    }

    let file_attrs = rules.file_attrs(&index_relpath, false);
    let conv = rules.conversion();
    let filter_fp = Some(rules.filter_process());
    let oid = if meta.file_type().is_symlink() {
        let target = fs::read_link(&abs).map_err(Error::Io)?;
        let data = crate::diff::symlink_target_bytes(&target);
        repo.odb().write(ObjectKind::Blob, &data)?
    } else {
        crate::diff::materialize_worktree_blob(
            repo.odb(),
            &abs,
            &meta,
            conv,
            &file_attrs,
            &index_relpath,
            None,
            filter_fp,
        )
        .map_err(|e| Error::Message(format!("could not store {index_relpath}: {e}")))?
    };
    let entry = entry_from_stat(&abs, index_relpath.as_bytes(), oid, mode)?;
    index.add_or_replace(entry);
    Ok(())
}

fn write_stash_tree_from_index(repo: &Repository, index: &mut Index) -> Result<ObjectId> {
    index.sort();
    let odb = repo.odb();
    let tree = write_tree_update_index(odb, index, "", WriteTreeFlags::silent())?;
    if is_empty_tree_oid(odb, &tree) {
        odb.write_local(ObjectKind::Tree, b"")?;
    }
    Ok(tree)
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
        Some(repo.git_dir()),
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
        if path_in_nested_git_worktree(work_tree, path) {
            continue;
        }
        let file_path = work_tree.join(path.trim_end_matches('/'));
        if file_path.is_dir() {
            if is_nested_git_worktree(&file_path) {
                continue;
            }
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
    i_tree: ObjectId,
    message: String,
    untracked_paths: Vec<String>,
    partial_patches: Option<Vec<FilePatch>>,
    push_pathspecs: Vec<String>,
    staged_only_paths: Vec<String>,
    mode: StashMode,
}

fn selection_is_active(options: &StashCreateOptions) -> bool {
    options
        .selection
        .as_ref()
        .is_some_and(|sel| !sel.ids.is_empty() || !sel.ranges.is_empty())
}

fn refresh_index_stat_from_worktree(
    index: &mut Index,
    work_tree: &Path,
    path: &str,
    oid: ObjectId,
    mode: u32,
) -> Result<()> {
    let abs = work_tree.join(path);
    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
    let entry = crate::index::entry_from_metadata(&meta, path.as_bytes(), oid, mode);
    index.add_or_replace(entry);
    Ok(())
}

fn apply_staged_worktree_only_reset(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    staged_paths: &[String],
) -> Result<()> {
    if staged_paths.is_empty() {
        return Ok(());
    }
    let head_entries = flatten_tree_full_for_repo(repo, head_tree, "")?;
    let index = repo.load_index()?;
    let worktree_rules = WorktreeRules::from_repository(repo, &index)?;
    for path in staged_paths {
        if let Some(entry) = flat_tree_lookup(&head_entries, path) {
            materialize_flat_entry_in_worktree(repo, work_tree, &worktree_rules, entry)?;
        } else {
            let file_path = work_tree.join(path);
            if file_path.is_dir() {
                if !is_nested_git_worktree(&file_path) {
                    let _ = fs::remove_dir_all(&file_path);
                }
            } else if file_path.exists() || file_path.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&file_path);
            }
            if let Some(parent) = file_path.parent() {
                remove_empty_dirs(parent, work_tree, repo.environment());
            }
        }
    }
    Ok(())
}

fn apply_staged_only_reset(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    staged_paths: &[String],
) -> Result<()> {
    if staged_paths.is_empty() {
        return Ok(());
    }
    let head_entries = flatten_tree_full_for_repo(repo, head_tree, "")?;
    let mut index = repo.load_index()?;
    let worktree_rules = WorktreeRules::from_repository(repo, &index)?;
    for path in staged_paths {
        if let Some(entry) = flat_tree_lookup(&head_entries, path) {
            materialize_flat_entry_in_worktree(repo, work_tree, &worktree_rules, entry)?;
            refresh_index_stat_from_worktree(&mut index, work_tree, path, entry.oid, entry.mode)?;
        } else {
            index.remove(path.as_bytes());
            let file_path = work_tree.join(path);
            if file_path.is_dir() {
                if !is_nested_git_worktree(&file_path) {
                    let _ = fs::remove_dir_all(&file_path);
                }
            } else if file_path.exists() || file_path.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&file_path);
            }
            if let Some(parent) = file_path.parent() {
                remove_empty_dirs(parent, work_tree, repo.environment());
            }
        }
    }
    repo.write_index(&mut index)?;
    Ok(())
}

fn tracked_stash_preview_paths(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    options: &StashCreateOptions,
) -> Result<Vec<String>> {
    let mut index = repo.load_index()?;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    if options.mode != StashMode::StagedOnly {
        let index_mtime = index_file_mtime(&repo.index_path());
        let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
        let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
        let diff_opts = DiffIndexToWorktreeOptions {
            index_mtime,
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: repo.config().ok(),
            worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
            ..Default::default()
        };
        let (unstaged, _) =
            diff_index_to_worktree_with_options(repo.odb(), &mut index, work_tree, diff_opts)?;
        for entry in unstaged {
            paths.insert(entry.path().to_string());
        }
    }
    for path in staged_change_paths_for_options(repo, &index, head_tree, options)? {
        paths.insert(path);
    }
    if let Some(ref patches) = resolve_partial_stash_patches(repo, options)? {
        for patch in patches {
            if let Some(path) = patch.new_path.as_ref().or(patch.old_path.as_ref()) {
                paths.insert(path.clone());
            }
        }
    }
    Ok(paths.into_iter().collect())
}

/// List paths that would be stashed without writing objects, refs, or worktree state.
///
/// Returns `None` when there is nothing to save.
///
/// # Errors
///
/// Propagates validation and diff failures.
pub fn preview_stash(
    repo: &Repository,
    options: &StashCreateOptions,
) -> Result<Option<StashPreview>> {
    options.validate()?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot stash in a bare repository".into()))?;
    let head = resolve_head_for_repository(repo)?;
    let head_oid = head_commit_oid(&head)?;
    let head_tree = repo.odb().commit_tree(&head_oid)?;

    if selection_is_active(options) {
        let patches = resolve_partial_stash_patches(repo, options)?.unwrap_or_default();
        if patches.is_empty() {
            return Ok(None);
        }
        let paths: Vec<String> = patches
            .into_iter()
            .filter_map(|p| p.new_path.or(p.old_path))
            .collect();
        return Ok(Some(StashPreview {
            paths,
            untracked: Vec::new(),
        }));
    }

    options.validate_staged_worktree(repo, work_tree, &head_tree)?;
    if !has_stashable_changes(repo, work_tree, &head_tree, options)? {
        return Ok(None);
    }

    let paths = tracked_stash_preview_paths(repo, work_tree, &head_tree, options)?;
    let untracked = if options.include_untracked && options.mode != StashMode::StagedOnly {
        collect_untracked_paths(repo, work_tree)?
    } else {
        Vec::new()
    };
    if paths.is_empty() && untracked.is_empty() {
        return Ok(None);
    }
    Ok(Some(StashPreview { paths, untracked }))
}

fn stash_push_pathspecs_active(options: &StashCreateOptions) -> bool {
    !options.pathspecs.is_empty() && !selection_is_active(options)
}

fn pathspec_matcher<'a>(repo: &'a Repository, pathspecs: &'a [String]) -> PathspecListMatcher<'a> {
    PathspecListMatcher::new(pathspecs, repo.pathspec_match_context())
}

fn index_stage0_paths(index: &Index) -> Vec<String> {
    let mut paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn validate_stash_push_pathspecs(
    repo: &Repository,
    work_tree: &Path,
    options: &StashCreateOptions,
) -> Result<()> {
    if !stash_push_pathspecs_active(options) {
        return Ok(());
    }
    let ctx = repo.pathspec_match_context();
    let index = repo.load_index()?;
    let index_owned = index_stage0_paths(&index);
    let index_paths: Vec<&str> = index_owned.iter().map(String::as_str).collect();
    for spec in &options.pathspecs {
        if pathspec_is_exclude(spec) {
            continue;
        }
        if crate::pathspec::sorted_paths_match_pathspec(spec, &index_paths, ctx) {
            continue;
        }
        if options.include_untracked {
            let untracked = collect_untracked_paths(repo, work_tree)?;
            let untracked_refs: Vec<&str> = untracked.iter().map(String::as_str).collect();
            if crate::pathspec::sorted_paths_match_pathspec(spec, &untracked_refs, ctx) {
                continue;
            }
            if !has_glob_chars(spec) && !spec.starts_with(':') && work_tree.join(spec).exists() {
                continue;
            }
        }
        return Err(Error::PathspecNoMatch { spec: spec.clone() });
    }
    Ok(())
}

fn filter_paths_by_pathspec(
    repo: &Repository,
    pathspecs: &[String],
    paths: Vec<String>,
) -> Vec<String> {
    let matcher = pathspec_matcher(repo, pathspecs);
    paths
        .into_iter()
        .filter(|path| matcher.matches(path))
        .collect()
}

fn has_stashable_changes_for_pathspecs(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    options: &StashCreateOptions,
) -> Result<bool> {
    let matcher = pathspec_matcher(repo, &options.pathspecs);
    let mut index = repo.load_index()?;
    let staged = diff_index_to_tree(repo.odb(), &index, Some(head_tree), true)?;
    if staged.iter().any(|e| matcher.matches(e.path())) {
        return Ok(true);
    }
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: true,
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        config: repo.config().ok(),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(repo.odb(), &mut index, work_tree, diff_opts)?;
    if unstaged.iter().any(|e| matcher.matches(e.path())) {
        return Ok(true);
    }
    if options.include_untracked {
        let untracked = filter_paths_by_pathspec(
            repo,
            &options.pathspecs,
            collect_untracked_paths(repo, work_tree)?,
        );
        if !untracked.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn apply_worktree_diff_to_index_for_pathspecs(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    pathspecs: &[String],
) -> Result<()> {
    let matcher = pathspec_matcher(repo, pathspecs);
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        ignore_submodule_untracked: true,
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        config: repo.config().ok(),
        worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(repo.odb(), index, work_tree, diff_opts)?;
    let mut worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let precompose = crate::precompose_config::effective_core_precomposeunicode_with_config(
        Some(repo.git_dir()),
        Some(&worktree_rules.config_arc()),
    );
    for entry in unstaged {
        let path = entry.path();
        if !matcher.matches(path) {
            continue;
        }
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

fn pathspecs_for_push_reset(
    repo: &Repository,
    head_tree: &ObjectId,
    pathspecs: &[String],
) -> Result<Vec<String>> {
    let protect = crate::path_validate::ProtectOptions::from_config(&*repo.config()?);
    let head_entries = flatten_tree_full(repo.odb(), head_tree, "", protect)?;
    let head_paths: Vec<&str> = head_entries.iter().map(|e| e.path.as_str()).collect();
    let index = repo.load_index()?;
    let index_owned = index_stage0_paths(&index);
    let index_paths: Vec<&str> = index_owned.iter().map(String::as_str).collect();
    let ctx = repo.pathspec_match_context();
    Ok(pathspecs
        .iter()
        .filter(|spec| {
            if pathspec_is_exclude(spec) {
                return false;
            }
            crate::pathspec::sorted_paths_match_pathspec(spec, &head_paths, ctx)
                || crate::pathspec::sorted_paths_match_pathspec(spec, &index_paths, ctx)
        })
        .cloned()
        .collect())
}

fn reset_stash_push_pathspecs_to_head(
    repo: &Repository,
    head_tree: &ObjectId,
    pathspecs: &[String],
) -> Result<()> {
    if pathspecs.is_empty() {
        return Ok(());
    }
    let opts = RestoreOptions {
        pathspecs: pathspecs.to_vec(),
        pathspec_sources: pathspecs.to_vec(),
        source: RestoreSource::Tree(*head_tree),
        staged: true,
        worktree: true,
        stage: None,
        remerge: None,
    };
    restore_paths(repo, &opts)?;
    Ok(())
}

fn merge_stash_index_trees(
    repo: &Repository,
    base_tree: &ObjectId,
    current_index_tree: &ObjectId,
    stash_index_tree: &ObjectId,
) -> Result<Option<ObjectId>> {
    if base_tree == stash_index_tree || current_index_tree == stash_index_tree {
        return Ok(None);
    }
    let output = merge_trees_three_way(
        repo,
        *base_tree,
        *current_index_tree,
        *stash_index_tree,
        MergeFavor::None,
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation {
            label_ours: "Current index",
            label_theirs: crate::merge_trees::TheirsConflictLabel::Fixed("Stashed index changes"),
            label_base: "Stash base",
            ..TreeMergeConflictPresentation::default()
        },
    )?;
    if output.index.entries.iter().any(|entry| entry.stage() != 0) {
        return Err(Error::StashIndexConflict);
    }
    Ok(Some(output.result_tree))
}

fn resolve_partial_stash_patches(
    repo: &Repository,
    options: &StashCreateOptions,
) -> Result<Option<Vec<FilePatch>>> {
    if !selection_is_active(options) {
        return Ok(None);
    }
    let Some(sel) = options.selection.as_ref() else {
        return Ok(None);
    };
    let diff_opts = diff_options_for_hunk_select(repo)?;
    let patches = resolve_selection(repo, DiffSide::Unstaged, sel, &options.pathspecs, diff_opts)?;
    Ok(Some(patches))
}

fn build_stash_commit(
    repo: &Repository,
    work_tree: &Path,
    options: &StashCreateOptions,
) -> Result<Option<BuiltStash>> {
    options.validate()?;
    let head = resolve_head_for_repository(repo)?;
    let head_oid = head_commit_oid(&head)?;
    let head_tree = repo.odb().commit_tree(&head_oid)?;
    options.validate_staged_worktree(repo, work_tree, &head_tree)?;

    let pathspec_push = stash_push_pathspecs_active(options);
    if pathspec_push {
        validate_stash_push_pathspecs(repo, work_tree, options)?;
    }

    let partial_patches = resolve_partial_stash_patches(repo, options)?;
    if let Some(ref patches) = partial_patches {
        if patches.is_empty() {
            return Ok(None);
        }
    } else if pathspec_push {
        if !has_stashable_changes_for_pathspecs(repo, work_tree, &head_tree, options)? {
            return Ok(None);
        }
    } else if !has_stashable_changes(repo, work_tree, &head_tree, options)? {
        return Ok(None);
    }

    let context = stash_context_message(repo, &head, &head_oid)?;
    let mut index = repo.load_index()?;
    let staged_only_paths = if options.mode == StashMode::StagedOnly {
        staged_change_paths_for_options(repo, &index, &head_tree, options)?
    } else {
        Vec::new()
    };
    if options.mode == StashMode::StagedOnly && staged_only_paths.is_empty() {
        return Ok(None);
    }
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
        let all = collect_untracked_paths(repo, work_tree)?;
        if pathspec_push {
            filter_paths_by_pathspec(repo, &options.pathspecs, all)
        } else {
            all
        }
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

    let w_tree = if options.mode == StashMode::StagedOnly {
        i_tree
    } else {
        let mut w_index = index_from_tree(repo, &i_tree)?;
        if let Some(ref patches) = partial_patches {
            apply_patch_to_index(
                repo,
                &mut w_index,
                patches,
                &IndexApplyOptions {
                    dry_run: false,
                    reverse: false,
                    head_tree_oid: None,
                },
            )
            .map_err(|e| Error::Message(e.to_string()))?;
        } else if pathspec_push {
            apply_worktree_diff_to_index_for_pathspecs(
                repo,
                work_tree,
                &mut w_index,
                &options.pathspecs,
            )?;
        } else {
            apply_worktree_diff_to_index(repo, work_tree, &mut w_index)?;
        }
        write_stash_tree_from_index(repo, &mut w_index)?
    };

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
        i_tree,
        message: w_message.trim_end().to_owned(),
        untracked_paths,
        partial_patches,
        push_pathspecs: if pathspec_push {
            options.pathspecs.clone()
        } else {
            Vec::new()
        },
        staged_only_paths,
        mode: options.mode,
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
    let work_tree = repo.work_tree().ok_or(Error::StashBareRepository)?;
    Ok(build_stash_commit(repo, work_tree, options)?.map(|b| b.w_commit))
}

/// Point `refs/stash` at `oid` and append a reflog entry with `message`.
///
/// # Errors
///
/// Propagates ref and reflog update failures.
pub fn store_stash(repo: &Repository, oid: ObjectId, message: &str, identity: &str) -> Result<()> {
    let old_oid = repo
        .resolve_ref_oid(STASH_REF)
        .unwrap_or_else(|_| ObjectId::zero());
    update_direct_ref_on_store(repo.refs(), repo.git_dir(), STASH_REF, &oid)?;
    append_reflog_on_store(
        repo.refs(),
        STASH_REF,
        &old_oid,
        &oid,
        identity,
        message,
        true,
        repo.config()?.as_ref(),
    )
}

/// Result of a successful [`push_stash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashPushResult {
    /// The W commit object id stored on `refs/stash`.
    pub oid: ObjectId,
    /// Reflog-style message stored on `refs/stash`.
    pub message: String,
    /// Tree recorded on the W commit.
    pub w_tree: ObjectId,
}

/// Create a stash entry, store it, and reset the worktree and index to `HEAD`.
///
/// Returns `None` when there is nothing to stash. Untracked paths included in
/// the stash are removed from the worktree after the reset.
///
/// # Errors
///
/// Same as [`create_stash`] and [`store_stash`], plus checkout failures.
pub fn push_stash(
    repo: &Repository,
    options: &StashCreateOptions,
) -> Result<Option<StashPushResult>> {
    if options.dry_run {
        return Err(Error::StashOptionConflict(
            "push_stash does not run with dry_run; use preview_stash".into(),
        ));
    }
    let work_tree = repo.work_tree().ok_or(Error::StashBareRepository)?;
    let Some(built) = build_stash_commit(repo, work_tree, options)? else {
        return Ok(None);
    };
    let result = StashPushResult {
        oid: built.w_commit,
        message: built.message.clone(),
        w_tree: built.w_tree,
    };
    store_stash(repo, built.w_commit, &built.message, &options.identity)?;
    if let Some(ref patches) = built.partial_patches {
        if !built.untracked_paths.is_empty() {
            remove_stashed_untracked(repo, work_tree, &built.untracked_paths)?;
        }
        reverse_apply_patches_to_worktree(
            repo,
            patches,
            &WorktreeReverseApplyOptions { dry_run: false },
        )
        .map_err(|e| Error::Message(e.to_string()))?;
    } else {
        let head = resolve_head_for_repository(repo)?;
        let head_oid = head_commit_oid(&head)?;
        let head_tree = repo.odb().commit_tree(&head_oid)?;
        if !built.untracked_paths.is_empty() {
            remove_stashed_untracked(repo, work_tree, &built.untracked_paths)?;
        }
        if !built.push_pathspecs.is_empty() {
            let reset_specs = pathspecs_for_push_reset(repo, &head_tree, &built.push_pathspecs)?;
            reset_stash_push_pathspecs_to_head(repo, &head_tree, &reset_specs)?;
            if built.mode == StashMode::StagedOnly {
                let index = repo.load_index()?;
                let all_staged = staged_change_paths(repo, &index, &head_tree)?;
                let stashed_set: HashSet<&str> =
                    built.staged_only_paths.iter().map(String::as_str).collect();
                let worktree_only: Vec<String> = all_staged
                    .into_iter()
                    .filter(|path| !stashed_set.contains(path.as_str()))
                    .collect();
                apply_staged_worktree_only_reset(repo, work_tree, &head_tree, &worktree_only)?;
            }
        } else {
            match built.mode {
                StashMode::StagedOnly => {
                    apply_staged_only_reset(repo, work_tree, &head_tree, &built.staged_only_paths)?;
                }
                StashMode::KeepIndex => {
                    checkout_between_trees(repo, Some(&built.w_tree), &built.i_tree)?;
                }
                StashMode::All => {
                    checkout_between_trees(repo, Some(&built.w_tree), &head_tree)?;
                    let mut head_index = index_from_tree_with_worktree_stats(repo, &head_tree)?;
                    repo.write_index(&mut head_index)?;
                }
            }
        }
    }
    Ok(Some(result))
}

/// List stash entries from `refs/stash` reflog (newest first).
///
/// # Errors
///
/// Propagates reflog read failures.
pub fn list_stashes(repo: &Repository) -> Result<Vec<StashEntry>> {
    let entries = read_reflog_on_store(repo.refs(), STASH_REF)?;
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

pub(crate) fn stash_oid_at(repo: &Repository, n: usize) -> Result<ObjectId> {
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
    let store = repo.refs();
    let entries = read_reflog_on_store(store, STASH_REF)?;
    if n >= entries.len() {
        return Err(Error::StashNotFound { n });
    }
    let _ = identity;
    delete_reflog_entries_rechain_on_store(store, STASH_REF, &[n])?;
    let remaining = read_reflog_on_store(store, STASH_REF)?;
    if let Some(top_entry) = remaining.last() {
        update_direct_ref_on_store(store, repo.git_dir(), STASH_REF, &top_entry.new_oid)?;
    } else {
        delete_ref_on_store(store, STASH_REF)?;
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
    Ok(pop_stash_entry(
        repo,
        work_tree,
        n,
        &StashApplyOptions { restore_index },
        identity,
    )?
    .has_conflicts())
}

/// Tree diff of stash W against its `HEAD` parent (`stash show` without `--index`).
///
/// When [`StashDiffOptions::include_untracked`] is set, paths from the U parent that were
/// not in the base tree are appended as [`DiffStatus::Added`] entries (matching
/// `git stash show -u`).
///
/// # Errors
///
/// Returns [`Error::StashNotFound`] or [`Error::CorruptStash`] when the entry is invalid.
pub fn stash_diff(
    repo: &Repository,
    n: usize,
    options: &StashDiffOptions,
) -> Result<Vec<DiffEntry>> {
    let oid = stash_oid_at(repo, n)?;
    let stash_commit = repo.odb().read_commit(&oid)?;
    let head_parent = stash_commit
        .parents
        .first()
        .ok_or(Error::CorruptStash("expected at least 2 parents"))?;
    let head_tree = repo.odb().commit_tree(head_parent)?;
    let mut diffs = diff_trees(repo.odb(), Some(&head_tree), Some(&stash_commit.tree), "")?;
    if options.include_untracked {
        append_stash_untracked_show_entries(repo, &head_tree, &stash_commit, &mut diffs)?;
    }
    Ok(diffs)
}

fn append_stash_untracked_show_entries(
    repo: &Repository,
    head_tree: &ObjectId,
    stash_commit: &CommitData,
    diffs: &mut Vec<DiffEntry>,
) -> Result<()> {
    if stash_commit.parents.len() < 3 {
        return Ok(());
    }
    let ut_oid = &stash_commit.parents[2];
    let ut_tree = repo.odb().commit_tree(ut_oid)?;
    let head_entries = flatten_tree_full_for_repo(repo, head_tree, "")?;
    let ut_entries = flatten_tree_full_for_repo(repo, &ut_tree, "")?;
    for entry in ut_entries {
        if let Some(idx) = diffs.iter().position(|d| d.path() == entry.path.as_str()) {
            if diffs[idx].status == DiffStatus::Deleted {
                let head_entry = flat_tree_lookup(&head_entries, &entry.path);
                let (old_oid, old_mode) = if let Some(h) = head_entry {
                    (h.oid, format_mode(h.mode))
                } else {
                    (zero_oid(), "000000".to_owned())
                };
                diffs[idx] = DiffEntry {
                    status: DiffStatus::Modified,
                    old_path: Some(entry.path.clone()),
                    new_path: Some(entry.path.clone()),
                    old_mode,
                    new_mode: format_mode(entry.mode),
                    old_oid,
                    new_oid: entry.oid,
                    score: None,
                };
            }
            continue;
        }
        diffs.push(DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(entry.path.clone()),
            old_mode: "000000".to_owned(),
            new_mode: format_mode(entry.mode),
            old_oid: zero_oid(),
            new_oid: entry.oid,
            score: None,
        });
    }
    diffs.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(())
}

/// Create `refs/heads/<name>` at the stash base commit, check it out, apply with `--index`, and drop on success.
///
/// # Errors
///
/// Returns [`BranchError::AlreadyExists`] when the branch name is taken,
/// [`Error::StashNotFound`] when `n` is out of range, and apply/switch failures otherwise.
pub fn stash_branch(
    repo: &Repository,
    name: &str,
    n: usize,
    identity: &str,
) -> Result<StashBranchOutcome> {
    validate_branch_short_name(name)
        .map_err(|e| Error::Message(format!("invalid branch name '{name}': {e}")))?;
    let branch_ref = format!("refs/heads/{name}");
    if repo.resolve_ref_oid(&branch_ref).is_ok() {
        return Err(BranchError::AlreadyExists {
            name: name.to_owned(),
        }
        .into());
    }
    let work_tree = repo.work_tree().ok_or(Error::StashBareRepository)?;
    let stash_oid = stash_oid_at(repo, n)?;
    let stash_commit = repo.odb().read_commit(&stash_oid)?;
    let base = *stash_commit
        .parents
        .first()
        .ok_or(Error::CorruptStash("expected at least 2 parents"))?;

    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Create {
                branch: name.to_owned(),
                start_oid: base,
                create_source_label: format!("stash@{{{n}}}"),
                track_upstream: None,
            },
            dry_run: false,
        },
    )?;

    let applied = apply_stash_entry(
        repo,
        work_tree,
        n,
        &StashApplyOptions {
            restore_index: true,
        },
    )?;
    if !applied.has_conflicts() {
        drop_stash(repo, n, identity)?;
    }
    Ok(StashBranchOutcome {
        branch: name.to_owned(),
        base,
        index: n,
        conflicts: applied.has_conflicts(),
        conflict_paths: applied.conflicted_paths,
    })
}

/// Delete `refs/stash` and its reflog, removing every stash entry.
///
/// # Returns
///
/// The number of reflog entries that were removed (0 when the reflog was already empty).
///
/// # Errors
///
/// Propagates ref store and reflog delete failures.
pub fn clear_stashes(repo: &Repository) -> Result<usize> {
    let store = repo.refs();
    let count = read_reflog_on_store(store, STASH_REF)?.len();
    let has_ref = repo.resolve_ref_oid(STASH_REF).is_ok();
    if count == 0 && !has_ref {
        return Ok(0);
    }
    if count > 0 {
        store.delete_reflog(STASH_REF)?;
    }
    if has_ref {
        delete_ref_on_store(store, STASH_REF)?;
    }
    Ok(count)
}

/// Write one flattened tree entry into the worktree (blob, symlink, or gitlink).
fn write_smudged_regular_file_replacing_symlink(
    repo: &Repository,
    rules: &WorktreeRules,
    rel_path: &str,
    file_path: &Path,
    mode: u32,
    blob: &[u8],
    oid: &ObjectId,
) -> Result<()> {
    let worktree_bytes = smudge_bytes_for_worktree_path(repo, rules, rel_path, mode, blob, oid)?;
    write_regular_file_replacing_symlink(file_path, &worktree_bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if mode == MODE_EXECUTABLE {
            let perms = fs::Permissions::from_mode(0o755);
            fs::set_permissions(file_path, perms)?;
        }
    }
    Ok(())
}

fn materialize_flat_entry_in_worktree(
    repo: &Repository,
    work_tree: &Path,
    rules: &WorktreeRules,
    entry: &FlatTreeEntry,
) -> Result<()> {
    let file_path = work_tree.join(&entry.path);
    if let Some(parent) = file_path.parent() {
        if let Ok(rel) = parent.strip_prefix(work_tree) {
            let mut cur = work_tree.to_path_buf();
            for comp in rel.components() {
                cur.push(comp);
                if cur.exists() && !cur.is_dir() {
                    let path = cur
                        .strip_prefix(work_tree)
                        .ok()
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .to_owned();
                    return Err(Error::StashCouldNotRestoreUntracked { path });
                }
            }
        }
        fs::create_dir_all(parent)?;
    }
    if entry.mode == MODE_GITLINK {
        fs::create_dir_all(&file_path)?;
        return Ok(());
    }
    let blob = repo.odb().read(&entry.oid)?;
    if entry.mode == MODE_SYMLINK {
        let target = String::from_utf8(blob.data).map_err(|_| Error::StashSymlinkNotUtf8)?;
        if file_path.exists() || file_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&file_path);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &file_path)?;
        #[cfg(not(unix))]
        fs::write(&file_path, target.as_bytes())?;
    } else {
        write_smudged_regular_file_replacing_symlink(
            repo,
            rules,
            &entry.path,
            &file_path,
            entry.mode,
            &blob.data,
            &entry.oid,
        )?;
    }
    Ok(())
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
    let stash_tree_entries = flatten_tree_full_for_repo(repo, &stash_commit.tree, "")?;
    let head_tree = repo.odb().commit_tree(head_at_stash)?;
    let base_tree_entries = flatten_tree_full_for_repo(repo, &head_tree, "")?;

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

    let config = repo.config()?;

    for path in stash_worktree_change_paths(repo, stash_commit)? {
        let Some(idx_entry) = current_index.get(path.as_bytes(), 0) else {
            continue;
        };
        if idx_entry.mode == MODE_GITLINK {
            continue;
        }
        if worktree_differs_from_index_entry(
            repo.odb(),
            repo.git_dir(),
            work_tree,
            idx_entry,
            true,
            Some(config.as_ref()),
            None,
        )? {
            return Err(Error::StashWouldOverwriteLocalChanges {
                paths: path.clone(),
            });
        }
    }
    Ok(())
}

/// Worktree paths that would block materializing the stash untracked-files parent.
fn collect_worktree_paths_blocking_stash_untracked_restore(
    repo: &Repository,
    work_tree: &Path,
) -> Result<Vec<String>> {
    let mut paths = HashSet::new();
    let mut index = repo.load_index()?;
    for entry in index.entries() {
        if entry.stage() != 0 {
            continue;
        }
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        let wt = work_tree.join(&path);
        if wt.exists() || wt.symlink_metadata().is_ok() {
            paths.insert(path);
        }
    }
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &index)?;
    let (untracked, ignored, _) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::Traditional,
            show_all: true,
            sort_paths: true,
            use_untracked_cache: false,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
        },
        &worktree_rules,
    )?;
    paths.extend(untracked);
    paths.extend(ignored);
    let mut sorted: Vec<String> = paths.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

/// Refuse when restoring the stash untracked-files parent would clobber an existing worktree path.
///
/// Must run before any worktree or index mutations in [`apply_stash`].
///
/// # Errors
///
/// Returns [`Error::StashCouldNotRestoreUntracked`] when a path from the untracked parent
/// collides with tracked, untracked, or ignored content already on disk.
pub(crate) fn check_stash_untracked_restore(
    repo: &Repository,
    work_tree: &Path,
    stash_commit: &CommitData,
) -> Result<()> {
    if stash_commit.parents.len() < 3 {
        return Ok(());
    }
    let ut_oid = &stash_commit.parents[2];
    let ut_tree = repo.odb().commit_tree(ut_oid)?;
    let ut_entries = flatten_tree_full_for_repo(repo, &ut_tree, "")?;
    if ut_entries.is_empty() {
        return Ok(());
    }
    let head_at_stash = stash_commit
        .parents
        .first()
        .ok_or(Error::CorruptStash("expected at least 2 parents"))?;
    let head_tree = repo.odb().commit_tree(head_at_stash)?;
    let base_tree_entries = flatten_tree_full_for_repo(repo, &head_tree, "")?;
    let stash_tree_entries = flatten_tree_full_for_repo(repo, &stash_commit.tree, "")?;
    let wt_changes = collect_flat_tree_worktree_changes(&base_tree_entries, &stash_tree_entries);
    let wt_deletions: HashSet<String> = wt_changes
        .iter()
        .filter(|(_, change)| change.is_none())
        .map(|(path, _)| path.clone())
        .collect();
    let ut_file_paths: HashSet<String> = ut_entries
        .iter()
        .filter(|e| e.mode != MODE_GITLINK && e.mode != MODE_TREE)
        .map(|e| e.path.clone())
        .collect();
    let existing_paths: Vec<String> =
        collect_worktree_paths_blocking_stash_untracked_restore(repo, work_tree)?
            .into_iter()
            .filter(|path| {
                for ut_file in &ut_file_paths {
                    if path != ut_file
                        && path.starts_with(&format!("{ut_file}/"))
                        && wt_deletions.contains(path)
                    {
                        return false;
                    }
                }
                true
            })
            .collect();
    let tree_changes: Vec<DiffEntry> = ut_entries
        .iter()
        .map(|entry| DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(entry.path.clone()),
            old_mode: String::new(),
            new_mode: format!("{:o}", entry.mode),
            old_oid: ObjectId::zero(),
            new_oid: entry.oid,
            score: None,
        })
        .collect();
    if let Some(path) =
        untracked_would_be_overwritten_by_tree_changes(&existing_paths, &tree_changes).first()
    {
        return Err(Error::StashCouldNotRestoreUntracked { path: path.clone() });
    }
    Ok(())
}

/// Apply a stash commit onto the current worktree and index.
///
/// Returns `true` if there were conflicts. Mirrors `git stash apply`:
/// three-way-merges changed files when HEAD has moved since the stash was
/// created, restores the index from the stash index parent when `restore_index`
/// is set (otherwise tracks current HEAD at touched paths), and materializes
/// any untracked-files parent. [`apply_stash_entry`] does the same by stash
/// index and also reports which paths conflicted.
pub fn apply_stash(
    repo: &Repository,
    work_tree: &Path,
    stash_oid: &ObjectId,
    restore_index: bool,
    _identity: &str,
) -> Result<bool> {
    Ok(apply_stash_commit(repo, work_tree, stash_oid, restore_index)?.has_conflicts())
}

/// Result of [`apply_stash_entry`] and [`pop_stash_entry`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StashApplyOutcome {
    /// Paths left with conflict stages (1–3) in the index, sorted and de-duplicated.
    ///
    /// Empty when the stash applied cleanly.
    pub conflicted_paths: Vec<String>,
}

impl StashApplyOutcome {
    /// Whether the apply left merge conflicts in the worktree and index.
    ///
    /// For [`pop_stash_entry`], `true` also means the stash entry was kept.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicted_paths.is_empty()
    }
}

/// Apply `stash@{{n}}` onto the current worktree and index without dropping it.
///
/// Same merge rules as [`apply_stash`]; see there for how `restore_index` is used.
///
/// # Parameters
///
/// - `repo`: the repository.
/// - `work_tree`: the worktree to apply into.
/// - `n`: stash index in newest-first order (`0` is `stash@{{0}}`).
/// - `restore_index`: restore the stashed index instead of keeping the current one.
///
/// # Returns
///
/// The paths that conflicted; [`StashApplyOutcome::has_conflicts`] is false on a clean apply.
///
/// # Errors
///
/// Returns [`Error::StashNotFound`] when `n` is out of range, and the [`apply_stash`]
/// errors (corrupt stash, local changes or untracked files in the way, index write).
pub fn apply_stash_entry(
    repo: &Repository,
    work_tree: &Path,
    n: usize,
    options: &StashApplyOptions,
) -> Result<StashApplyOutcome> {
    let oid = stash_oid_at(repo, n)?;
    apply_stash_commit(repo, work_tree, &oid, options.restore_index)
}

/// Apply `stash@{{n}}` and drop it when it applied without conflicts.
///
/// # Parameters
///
/// - `repo`, `work_tree`, `n`, `restore_index`: as for [`apply_stash_entry`].
/// - `identity`: reflog identity for the drop.
///
/// # Returns
///
/// The conflicted paths. When there are any, the entry is kept on the stash.
///
/// # Errors
///
/// Propagates [`apply_stash_entry`] and [`drop_stash`] failures.
pub fn pop_stash_entry(
    repo: &Repository,
    work_tree: &Path,
    n: usize,
    options: &StashApplyOptions,
    identity: &str,
) -> Result<StashApplyOutcome> {
    let outcome = apply_stash_entry(repo, work_tree, n, options)?;
    if !outcome.has_conflicts() {
        drop_stash(repo, n, identity)?;
    }
    Ok(outcome)
}

/// Sorted, de-duplicated paths of every conflict-stage entry in `index`.
fn unmerged_paths(index: &Index) -> Vec<String> {
    let mut paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() != 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn apply_stash_commit(
    repo: &Repository,
    work_tree: &Path,
    stash_oid: &ObjectId,
    restore_index: bool,
) -> Result<StashApplyOutcome> {
    let stash_commit = repo.odb().read_commit(stash_oid)?;

    if stash_commit.parents.len() < 2 {
        return Err(Error::CorruptStash("expected at least 2 parents"));
    }

    let head_at_stash = &stash_commit.parents[0];
    let index_commit_oid = &stash_commit.parents[1];

    let merged_index_tree = if restore_index {
        let head_at_stash_tree = repo.odb().commit_tree(head_at_stash)?;
        let idx_tree = repo.odb().commit_tree(index_commit_oid)?;
        let mut snapshot = match repo.load_index() {
            Ok(idx) => idx,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
            Err(e) => return Err(e),
        };
        let current_index_tree = write_stash_tree_from_index(repo, &mut snapshot)?;
        merge_stash_index_trees(repo, &head_at_stash_tree, &current_index_tree, &idx_tree)?
    } else {
        None
    };

    check_stash_apply_would_overwrite_local_changes(repo, work_tree, &stash_commit)?;
    check_stash_untracked_restore(repo, work_tree, &stash_commit)?;

    // Load current index
    let current_index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };

    // Read stash trees
    let stash_tree_entries = flatten_tree_full_for_repo(repo, &stash_commit.tree, "")?;

    // Read HEAD-at-stash tree (base)
    let head_at_stash_tree = repo.odb().commit_tree(head_at_stash)?;
    let base_tree_entries = flatten_tree_full_for_repo(repo, &head_at_stash_tree, "")?;

    let wt_changes = collect_flat_tree_worktree_changes(&base_tree_entries, &stash_tree_entries);

    // Read index commit tree
    let idx_tree = repo.odb().commit_tree(index_commit_oid)?;
    let idx_tree_entries = flatten_tree_full_for_repo(repo, &idx_tree, "")?;
    // Determine if HEAD has moved since the stash was created
    let current_head = resolve_head_for_repository(repo)?;
    let current_head_oid = current_head.oid().copied();
    let head_moved = current_head_oid.as_ref() != Some(head_at_stash);

    // Current HEAD tree (three-way merge when HEAD moved; index reset without `--index`).
    let current_head_loaded: Vec<FlatTreeEntry>;
    let current_head_entries: &[FlatTreeEntry] = if head_moved {
        let Some(head_oid) = current_head_oid.as_ref() else {
            return Err(Error::StashMissingHead);
        };
        let head_tree = repo.odb().commit_tree(head_oid)?;
        current_head_loaded = flatten_tree_full_for_repo(repo, &head_tree, "")?;
        &current_head_loaded
    } else {
        &base_tree_entries
    };

    let mut has_conflicts = false;
    let mut new_index = current_index.clone();
    let worktree_rules = WorktreeRules::from_repository(repo, &new_index)?;

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

                let stash_blob = repo.odb().read(&entry.oid)?;

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
                        .and_then(|e| repo.odb().read(&e.oid).ok())
                        .map(|o| o.data)
                        .unwrap_or_default();
                    let ours_content = flat_tree_lookup(current_head_entries, path)
                        .and_then(|e| repo.odb().read(&e.oid).ok())
                        .map(|o| o.data)
                        .unwrap_or_default();
                    let theirs_content = stash_blob.data;

                    // If ours == base, no conflict (only stash changed this file)
                    if ours_content == base_content {
                        write_smudged_regular_file_replacing_symlink(
                            repo,
                            &worktree_rules,
                            path,
                            &file_path,
                            entry.mode,
                            &theirs_content,
                            &entry.oid,
                        )?;
                    } else if ours_content == theirs_content {
                        // Both changed the same way, no conflict
                        let smudge_oid = flat_tree_lookup(current_head_entries, path)
                            .map(|e| e.oid)
                            .unwrap_or(entry.oid);
                        write_smudged_regular_file_replacing_symlink(
                            repo,
                            &worktree_rules,
                            path,
                            &file_path,
                            entry.mode,
                            &ours_content,
                            &smudge_oid,
                        )?;
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
                        let marker_oid = repo.odb().hash(ObjectKind::Blob, &output.content)?;
                        let worktree_bytes = smudge_bytes_for_worktree_path(
                            repo,
                            &worktree_rules,
                            path,
                            entry.mode,
                            &output.content,
                            &marker_oid,
                        )?;
                        write_regular_file_replacing_symlink(&file_path, &worktree_bytes)?;
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
                    write_smudged_regular_file_replacing_symlink(
                        repo,
                        &worktree_rules,
                        path,
                        &file_path,
                        entry.mode,
                        &stash_blob.data,
                        &entry.oid,
                    )?;
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

    if restore_index && !has_conflicts {
        if let Some(merged_tree) = merged_index_tree {
            new_index = index_from_tree_with_worktree_stats(repo, &merged_tree)?;
        } else {
            // Stash index matched current index or HEAD at stash time; replay per-path index parent.
            for base_entry in &base_tree_entries {
                if flat_tree_lookup(&idx_tree_entries, &base_entry.path).is_none() {
                    new_index.remove(base_entry.path.as_bytes());
                }
            }
            for idx_entry in &idx_tree_entries {
                let path = &idx_entry.path;
                let base_oid = flat_tree_lookup(&base_tree_entries, path).map(|e| &e.oid);
                if base_oid != Some(&idx_entry.oid) {
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
            for (path, _) in &wt_changes {
                if let Some(ie) = new_index.get_mut(path.as_bytes(), 0) {
                    ie.set_skip_worktree(false);
                }
            }
            new_index.sort();
        }
    } else if !restore_index {
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
                    repo.odb().read(&te.oid)?.data.len() as u32
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
                            repo.odb().read(&ie.oid)?.data.len() as u32
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
            repo.odb(),
            repo.git_dir(),
            &mut new_index,
            work_tree,
            None,
            repo.config().ok().as_deref(),
            None,
            rules_arc.as_ref(),
        )?;
    }
    let conflicted_paths = if has_conflicts {
        unmerged_paths(&new_index)
    } else {
        Vec::new()
    };
    repo.write_index(&mut new_index)
        .map_err(|e| Error::StashIndexWrite(e.to_string()))?;

    // Apply untracked files if present (3rd parent)
    if stash_commit.parents.len() >= 3 {
        let ut_oid = &stash_commit.parents[2];
        let ut_tree = repo.odb().commit_tree(ut_oid)?;
        let ut_entries = flatten_tree_full_for_repo(repo, &ut_tree, "")?;
        for entry in &ut_entries {
            materialize_flat_entry_in_worktree(repo, work_tree, &worktree_rules, entry)?;
        }
    }

    Ok(StashApplyOutcome { conflicted_paths })
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
pub fn drop_stash_commit(repo: &Repository, stash_oid: &ObjectId) -> Result<bool> {
    let store = repo.refs();
    let mut entries = read_reflog_on_store(store, STASH_REF)?;
    if entries.last().is_none_or(|tip| tip.new_oid != *stash_oid) {
        return Ok(false);
    }
    entries.pop();
    match entries.last().map(|entry| entry.new_oid) {
        None => delete_ref_on_store(store, STASH_REF)?,
        Some(prev) => {
            store.replace_reflog(STASH_REF, entries)?;
            update_direct_ref_on_store(store, repo.git_dir(), STASH_REF, &prev)?;
        }
    }
    Ok(true)
}

/// Stash dirty worktree/index changes before `pull --autostash` / merge autostash.
///
/// Returns `None` when there is nothing to stash.
///
/// # Errors
///
/// Propagates stash creation and checkout failures.
pub fn push_operation_autostash(repo: &Repository, identity: &str) -> Result<Option<ObjectId>> {
    Ok(push_stash(
        repo,
        &StashCreateOptions {
            message: Some("autostash".to_owned()),
            include_untracked: true,
            identity: identity.to_owned(),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        },
    )?
    .map(|r| r.oid))
}

/// Re-apply an operation autostash after a successful pull/merge/rebase integration.
///
/// # Returns
///
/// `true` when applying the autostash left conflicts (the stash entry is kept).
///
/// # Errors
///
/// Propagates stash apply failures.
pub fn apply_operation_autostash(
    repo: &Repository,
    stash_oid: &ObjectId,
    identity: &str,
) -> Result<bool> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("cannot apply autostash in a bare repository".into()))?;
    let conflicts = apply_stash(repo, work_tree, stash_oid, false, identity)?;
    if !conflicts {
        let _ = drop_stash_commit(repo, stash_oid)?;
    }
    Ok(conflicts)
}

/// Put a stashed autostash back after an aborted pull/merge (integration did not finish).
///
/// # Errors
///
/// Propagates stash apply failures.
pub fn restore_operation_autostash(
    repo: &Repository,
    stash_oid: &ObjectId,
    identity: &str,
) -> Result<()> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("cannot restore autostash in a bare repository".into()))?;
    let conflicts = apply_stash(repo, work_tree, stash_oid, false, identity)?;
    if !conflicts {
        let _ = drop_stash_commit(repo, stash_oid)?;
    }
    Ok(())
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
    let path = repo.git_dir().join("MERGE_AUTOSTASH");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(false);
    }
    let stash_oid = ObjectId::from_hex(trimmed)?;
    let work_tree = repo.work_tree().ok_or_else(|| {
        Error::Message("cannot apply merge autostash in a bare repository".into())
    })?;
    let conflicts = apply_stash(repo, work_tree, &stash_oid, false, "")?;
    let _ = fs::remove_file(&path);
    if !conflicts {
        let _ = drop_stash_commit(repo, &stash_oid)?;
    }
    Ok(conflicts)
}

#[cfg(test)]
mod coverage_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::index::{entry_from_stat, MODE_REGULAR};
    use crate::refs;
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
        let repo =
            Repository::open(&git, &RepositoryOptions::empty().with_work_tree(root)).unwrap();
        let wt = root.join("f");
        fs::write(&wt, b"base\n").unwrap();
        let mut index = repo.load_index().unwrap();
        let oid = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        index.add_or_replace(entry_from_stat(&wt, b"f", oid, MODE_REGULAR).unwrap());
        index.sort();
        repo.write_index(&mut index).unwrap();
        let tree = write_stash_tree_from_index(&repo, &mut index).unwrap();
        let parent = refs::resolve_ref(repo.git_dir(), "HEAD").ok();
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
        refs::write_ref(repo.git_dir(), "HEAD", &commit_oid).unwrap();
        refs::write_ref(repo.git_dir(), "refs/heads/main", &commit_oid).unwrap();
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
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        let oid = create_stash(&repo, &opts).expect("create").expect("some");
        store_stash(&repo, oid, "On main: msg", ident).expect("store");
        let listed = list_stashes(&repo).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].oid, oid);
        let diffs = stash_diff(&repo, 0, &StashDiffOptions::default()).expect("diff");
        assert!(!diffs.is_empty());
        drop_stash(&repo, 0, ident).expect("drop");
        assert!(list_stashes(&repo).expect("list").is_empty());
    }

    #[test]
    fn push_stash_refreshes_index_stat_for_clean_status() {
        use crate::porcelain::status::{status, StatusOptions};
        use crate::progress::NullProgress;

        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        fs::write(tmp.path().join("f"), b"base\n").unwrap();
        fs::write(tmp.path().join("g"), b"also\n").unwrap();
        let mut index = repo.load_index().unwrap();
        for (path, bytes) in [("f", b"base\n" as &[u8]), ("g", b"also\n")] {
            let oid = repo.odb().write(ObjectKind::Blob, bytes).unwrap();
            index.add_or_replace(
                entry_from_stat(&tmp.path().join(path), path.as_bytes(), oid, MODE_REGULAR)
                    .unwrap(),
            );
        }
        index.sort();
        repo.write_index(&mut index).unwrap();
        let tree = write_stash_tree_from_index(&repo, &mut index).unwrap();
        let commit = CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "two files\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let head = write_commit_object(&repo, &commit, None).unwrap();
        refs::write_ref(repo.git_dir(), "HEAD", &head).unwrap();
        refs::write_ref(repo.git_dir(), "refs/heads/main", &head).unwrap();

        fs::write(tmp.path().join("f"), b"changed\n").unwrap();
        let opts = StashCreateOptions {
            message: None,
            include_untracked: false,
            identity: "T <t@t> 1 +0000".into(),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        push_stash(&repo, &opts).expect("push").expect("oid");

        let index = repo.load_index().unwrap();
        assert!(index.get(b"f", 0).expect("f").size > 0);
        assert!(index.get(b"g", 0).expect("g").size > 0);
        let st = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");
        assert!(
            st.unstaged.is_empty(),
            "worktree matches HEAD after stash push"
        );
        assert!(st.staged.is_empty());
    }

    #[test]
    fn push_stash_returns_none_when_clean() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let opts = StashCreateOptions {
            message: None,
            include_untracked: false,
            identity: "T <t@t> 1 +0000".into(),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        assert!(push_stash(&repo, &opts).expect("push").is_none());
    }

    #[test]
    fn check_stash_untracked_restore_rejects_collision() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let ident = "T <t@t> 1 +0000";
        fs::write(tmp.path().join("u.txt"), b"stashed\n").unwrap();
        let opts = StashCreateOptions {
            message: Some("m".into()),
            include_untracked: true,
            identity: ident.into(),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        push_stash(&repo, &opts).expect("push").expect("oid");
        fs::write(tmp.path().join("u.txt"), b"precious\n").unwrap();
        let oid = list_stashes(&repo).expect("list")[0].oid;
        let stash_commit = repo.odb().read_commit(&oid).unwrap();
        let err = super::check_stash_untracked_restore(&repo, tmp.path(), &stash_commit)
            .expect_err("collision");
        assert!(matches!(
            err,
            Error::StashCouldNotRestoreUntracked { ref path } if path == "u.txt"
        ));
    }
}

#[cfg(test)]
mod hunk_push_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide};
    use crate::diff::{unstaged_file_diffs_with_peer_hunks, DiffOptions};
    use crate::environment::RepositoryOptions;
    use crate::porcelain::hunk_select::HunkSelection;
    use crate::porcelain::index_from_tree::index_from_tree;
    use crate::repo::Repository;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn open_repo(root: &std::path::Path) -> Repository {
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover")
    }

    fn two_hunk_file() -> (String, String) {
        let mut base = String::new();
        for i in 0..24 {
            base.push_str(&format!("line{i}\n"));
        }
        let mut edited = base.clone();
        edited = edited.replacen("line5\n", "LINE5\n", 1);
        edited = edited.replacen("line18\n", "LINE18\n", 1);
        (base, edited)
    }

    fn tree_blob(repo: &Repository, tree_oid: &ObjectId, path: &str) -> Vec<u8> {
        let index = index_from_tree(repo, tree_oid).expect("tree index");
        let entry = index.get(path.as_bytes(), 0).expect("path in tree");
        repo.odb().read(&entry.oid).expect("blob").data
    }

    fn stash_w_tree(repo: &Repository, stash_oid: &ObjectId) -> ObjectId {
        repo.odb().commit_tree(stash_oid).unwrap()
    }

    fn hunk_selection_for_first_hunk(repo: &Repository) -> HunkSelection {
        let (files, peers) =
            unstaged_file_diffs_with_peer_hunks(repo, &[], DiffOptions::default()).unwrap();
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            "f.txt",
            &files[0].hunks,
            peers.get("f.txt").map(|v| v.as_slice()).unwrap_or(&[]),
        );
        HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        }
    }

    const IDENT: &str = "Test <t@example.com> 1000000000 +0000";

    fn setup_two_hunk_repo() -> (TempDir, Repository, HunkSelection) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main"]);
        let (base, edited) = two_hunk_file();
        fs::write(root.join("f.txt"), base).unwrap();
        git(root, &["add", "f.txt"]);
        git(root, &["commit", "-qm", "init"]);
        let repo = open_repo(root);
        fs::write(root.join("f.txt"), edited).unwrap();
        let selection = hunk_selection_for_first_hunk(&repo);
        (tmp, repo, selection)
    }

    #[test]
    fn stash_push_hunks_dry_run_does_not_mutate_repo() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        let before = fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        let preview = preview_stash(
            &repo,
            &StashCreateOptions {
                message: Some("preview".into()),
                include_untracked: false,
                identity: IDENT.into(),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: true,
                mode: StashMode::default(),
            },
        )
        .expect("preview")
        .expect("paths");
        assert!(preview.paths.iter().any(|p| p == "f.txt"));
        assert_eq!(list_stashes(&repo).expect("list").len(), 0);
        assert_eq!(
            fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
            before,
            "dry-run must not change worktree"
        );
    }

    #[test]
    fn stash_push_hunks_stashes_only_selected() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        let oid = push_stash(
            &repo,
            &StashCreateOptions {
                message: Some("partial".into()),
                include_untracked: false,
                identity: IDENT.into(),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
        )
        .expect("push")
        .expect("oid")
        .oid;
        let w_tree = stash_w_tree(&repo, &oid);
        let blob = tree_blob(&repo, &w_tree, "f.txt");
        let text = String::from_utf8_lossy(&blob);
        assert!(text.contains("LINE5"), "stashed hunk in W tree: {text}");
        assert!(
            !text.contains("LINE18"),
            "unselected hunk must not be in stash tree: {text}"
        );
        git(tmp.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn stash_push_hunks_worktree_keeps_rest() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        push_stash(
            &repo,
            &StashCreateOptions {
                message: None,
                include_untracked: false,
                identity: IDENT.into(),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
        )
        .expect("push")
        .expect("oid");
        let on_disk = fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert!(
            on_disk.contains("line5\n") && !on_disk.contains("LINE5"),
            "first hunk reverted toward index: {on_disk}"
        );
        assert!(
            on_disk.contains("LINE18"),
            "second hunk still unstaged on disk: {on_disk}"
        );
        git(tmp.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn stash_push_hunks_roundtrip_with_git_stash_apply() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        push_stash(
            &repo,
            &StashCreateOptions {
                message: None,
                include_untracked: false,
                identity: IDENT.into(),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
        )
        .expect("push")
        .expect("oid");
        git(
            tmp.path(),
            &["restore", "--worktree", "--source=HEAD", "--", "f.txt"],
        );
        git(tmp.path(), &["stash", "apply"]);
        let on_disk = fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert!(on_disk.contains("LINE5"), "git stash apply restored hunk");
        git(tmp.path(), &["fsck", "--strict"]);
    }

    #[test]
    fn stash_push_hunks_pop_restores() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        push_stash(
            &repo,
            &StashCreateOptions {
                message: None,
                include_untracked: false,
                identity: IDENT.into(),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
        )
        .expect("push")
        .expect("oid");
        git(
            tmp.path(),
            &["restore", "--worktree", "--source=HEAD", "--", "f.txt"],
        );
        git(tmp.path(), &["stash", "pop"]);
        let on_disk = fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert!(
            on_disk.contains("LINE5"),
            "pop restored stashed hunk: {on_disk}"
        );
        git(tmp.path(), &["fsck", "--strict"]);
    }
}
