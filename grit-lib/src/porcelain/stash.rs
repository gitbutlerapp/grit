//! Stash option and result types plus the Git-compatible stash engine.
//!
//! The library builds Git-compatible stash commits (W/I/U parents), updates
//! `refs/stash` and its reflog, and applies entries onto the worktree and index.
//! The operations are reached through
//! [`RepositoryStash`](crate::repo_handles::RepositoryStash) (`repo.stash()`); apply and
//! pop report the paths left conflicted, with their kinds, in a [`StashApplyOutcome`] (a pop keeps the entry
//! when there are any).
//!
//! Flattening and worktree helpers ([`FlatTreeEntry`], [`flatten_tree_full`],
//! [`add_stage_entry`], [`worktree_bytes_for_index_mode`],
//! [`write_regular_file_replacing_symlink`], [`remove_empty_dirs`]) are shared
//! building blocks for apply and for embedders that manipulate stash trees.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, zero_oid, DiffEntry,
    DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{ApplyError, Error, Result};
use crate::index::{
    index_file_mtime, Index, IndexEntry, IndexLock, MODE_GITLINK, MODE_SYMLINK, MODE_TREE,
};
use crate::objects::FileMode;
use crate::worktree::{map_worktree_io, WorktreePathOperation};
// `MODE_EXECUTABLE` only drives the Unix executable-bit application below.
use crate::apply::{apply_patch_to_index, IndexApplyOptions};
use crate::check_ref_format::validate_branch_short_name;
use crate::diff::hunk_id::HunkId;
use crate::ident::Signature;
#[cfg(unix)]
use crate::index::MODE_EXECUTABLE;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, DirectoryRenameMode, TreeMergeConflictPresentation,
    WhitespaceMergeOptions,
};
use crate::objects::{parse_tree, CommitData, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::operation::{conflicts_from_index, Conflict};
use crate::pathspec::{
    has_glob_chars, matches_pathspec_list, pathspec_is_exclude, PathspecListMatcher,
};
use crate::porcelain::branch_error::BranchError;
use crate::porcelain::checkout::is_eligible_for_parallel_checkout;
use crate::porcelain::checkout::smudge_bytes_for_worktree_path;
use crate::porcelain::checkout::{checkout_between_trees_locked, CheckoutIndexWrite};
use crate::porcelain::clean::{is_nested_git_worktree, path_in_nested_git_worktree};
use crate::porcelain::commit::write_commit_object;
use crate::porcelain::hunk_select::{
    resolve_unstaged_selection, reverse_apply_patches_to_worktree, HunkSelection,
    ResolvedSelection, WorktreeReverseApplyOptions,
};
use crate::porcelain::index_from_tree::{
    index_from_tree_with_seeded_stats, index_from_tree_with_seeded_worktree_stats,
    index_from_tree_with_worktree_stats,
};
use crate::porcelain::restore::{restore_paths_with_lock, RestoreOptions, RestoreSource};
use crate::porcelain::status::{
    collect_untracked_and_ignored_inner, IgnoredMode, UntrackedCacheMissAction, UntrackedScan,
};
use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use crate::porcelain::worktree_guard::untracked_would_be_overwritten_by_tree_changes;
use crate::progress::{
    poll_cancel, Cancel, NeverCancel, NullProgress, ProgressPhase, ProgressScope, ProgressSink,
};
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

/// Changed tree leaves retain raw Git path bytes, independently of diff's string encoding.
#[derive(Clone)]
struct StashTreeEntry {
    path: Vec<u8>,
    mode: u32,
    oid: ObjectId,
}

impl StashTreeEntry {
    fn lookup<'a>(entries: &'a [Self], path: &[u8]) -> Option<&'a Self> {
        entries
            .binary_search_by(|e| e.path.as_slice().cmp(path))
            .ok()
            .map(|i| &entries[i])
    }
}

/// Metadata is retained only for changed leaves; identical subtree OIDs are skipped.
struct StashTreeDelta {
    base: Vec<StashTreeEntry>,
    changes: Vec<(Vec<u8>, Option<StashTreeEntry>)>,
}

impl StashTreeDelta {
    fn read(repo: &Repository, base: &ObjectId, other: &ObjectId) -> Result<Self> {
        let protect = crate::path_validate::ProtectOptions::from_config(&*repo.config()?);
        let mut before = Vec::new();
        let mut changes = Vec::new();
        for change in diff_trees(repo.odb(), Some(base), Some(other), "")? {
            let entry = |path: String, mode: FileMode, oid| -> Result<StashTreeEntry> {
                let path = crate::repo_relpath::storage_bytes(&path).into_owned();
                let mode = mode.bits();
                crate::path_population::validate_worktree_path(&path, mode, protect)?;
                Ok(StashTreeEntry { path, mode, oid })
            };
            if let Some(path) = &change.old_path {
                before.push(entry(path.clone(), change.old_mode, change.old_oid)?);
            }
            let after = change
                .new_path
                .map(|path| entry(path, change.new_mode, change.new_oid))
                .transpose()?;
            if let Some(path) = after.as_ref().map(|e| e.path.clone()).or_else(|| {
                change
                    .old_path
                    .map(|path| crate::repo_relpath::storage_bytes(&path).into_owned())
            }) {
                changes.push((path, after));
            }
        }
        before.sort_by(|a, b| a.path.cmp(&b.path));
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Self {
            base: before,
            changes,
        })
    }
}

/// Look up affected paths through the ODB's bounded parsed-tree cache.
fn stash_tree_entry_at_path(
    repo: &Repository,
    mut tree: ObjectId,
    path: &[u8],
) -> Result<Option<StashTreeEntry>> {
    let mut components = path.split(|&b| b == b'/').peekable();
    while let Some(name) = components.next() {
        let entries = repo.odb().cached_tree_entries(&tree)?;
        let Some(entry) = entries.iter().find(|e| e.name == name) else {
            return Ok(None);
        };
        if components.peek().is_none() {
            // A directory in HEAD is not a file at this path.
            return Ok((entry.mode != MODE_TREE).then(|| StashTreeEntry {
                path: path.to_owned(),
                mode: entry.mode,
                oid: entry.oid,
            }));
        }
        if entry.mode != MODE_TREE {
            return Ok(None);
        }
        tree = entry.oid;
    }
    Ok(None)
}

fn load_stash_index(repo: &Repository) -> Result<Index> {
    match repo.load_index() {
        Ok(index) => Ok(index),
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(Index::new()),
        Err(e) => Err(e),
    }
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
    use std::io::Write;
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        fs::remove_file(path)?;
    }
    if path.is_file() {
        if let Ok(mut file) = fs::OpenOptions::new().write(true).truncate(true).open(path) {
            file.write_all(contents)?;
            return Ok(());
        }
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

/// Which local changes a stash push saves and how the worktree/index are left afterward.
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

/// Paths that would be recorded by
/// [`RepositoryStash::preview`](crate::repo_handles::RepositoryStash::preview).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashPreview {
    /// Repository-relative paths with tracked changes that would be stashed.
    pub paths: Vec<String>,
    /// Untracked paths that would be included when `include_untracked` is set.
    pub untracked: Vec<String>,
    /// For a partial push ([`StashPushOptions::selection`]), the hunk ids that would be stashed,
    /// per repository-relative path. Empty otherwise.
    pub stashed_hunks: BTreeMap<String, Vec<HunkId>>,
    /// For a partial push, the requested line range labels (`start` or `start-end`) that would
    /// be stashed, per path. Empty otherwise.
    pub stashed_lines: BTreeMap<String, Vec<String>>,
}

/// Options for [`RepositoryStash::push`](crate::repo_handles::RepositoryStash::push),
/// [`RepositoryStash::create`](crate::repo_handles::RepositoryStash::create), and
/// [`RepositoryStash::preview`](crate::repo_handles::RepositoryStash::preview).
#[derive(Debug, Clone)]
pub struct StashPushOptions {
    /// Reflog/stash message override; when set, the W commit message is `On <branch>: <msg>`.
    pub message: Option<String>,
    /// When true, include untracked files as a parentless U commit (parent 3 of W).
    pub include_untracked: bool,
    /// Author and committer of the stash commits and identity of the `refs/stash` reflog entry
    /// (for example from [`Repository::identity`]).
    pub identity: Signature,
    /// When set, stash only these unstaged hunks (index is left unchanged on push).
    pub selection: Option<HunkSelection>,
    /// Pathspecs narrowing hunk resolution (same as `grit add --hunks`).
    pub pathspecs: Vec<String>,
    /// When true, [`RepositoryStash::push`](crate::repo_handles::RepositoryStash::push) refuses
    /// to run; use [`RepositoryStash::preview`](crate::repo_handles::RepositoryStash::preview),
    /// which ignores this flag.
    pub dry_run: bool,
    /// Push mode (`--staged`, `--keep-index`, or default).
    pub mode: StashMode,
}

impl StashPushOptions {
    /// Reject flag combinations Git and grit do not support.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StashOptionConflict`] when options conflict and
    /// [`Error::InvalidSignature`] when [`Self::identity`] cannot be written into a commit.
    pub fn validate(&self) -> Result<()> {
        self.identity.validate()?;
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
        if self.include_untracked && selection_is_active(self) {
            return Err(Error::StashOptionConflict(
                "cannot combine --include-untracked with --hunks or --lines".into(),
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

/// Options for [`RepositoryStash::apply`](crate::repo_handles::RepositoryStash::apply) and
/// [`RepositoryStash::pop`](crate::repo_handles::RepositoryStash::pop).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StashApplyOptions {
    /// Restore the stashed index parent instead of keeping the current index at touched paths.
    pub restore_index: bool,
}

/// Options for [`RepositoryStash::diff`](crate::repo_handles::RepositoryStash::diff).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StashDiffOptions {
    /// Include untracked files from the stash U parent (same as `git stash show -u`).
    pub include_untracked: bool,
}

/// Result of [`RepositoryStash::branch`](crate::repo_handles::RepositoryStash::branch).
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

/// One entry from [`RepositoryStash::list`](crate::repo_handles::RepositoryStash::list)
/// (`stash@{{index}}` in newest-first order).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
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
    options: &StashPushOptions,
) -> Result<Vec<String>> {
    let paths = staged_change_paths(repo, index, head_tree)?;
    Ok(filter_paths_by_pathspecs(paths, &options.pathspecs))
}

/// What [`has_stashable_changes`] found.
enum StashableChanges {
    /// Index and worktree match `HEAD`, and no wanted untracked files exist.
    Nothing,
    /// There is something to stash. `unstaged` is the index-to-worktree diff when the scan
    /// computed one over an index that [`index_is_plain_for_stash`] accepts, so building the W
    /// tree can reuse it instead of diffing the worktree again.
    Found { unstaged: Option<Vec<DiffEntry>> },
}

/// Whether diffing `index` against the worktree gives the same result as diffing an index
/// built from the tree `index` writes, seeded with its stat data: only plain stage-0 file
/// entries, none of which carries a flag that a tree-built copy would lose.
fn index_is_plain_for_stash(index: &Index) -> bool {
    index.entries.iter().all(|e| {
        e.stage() == 0
            && e.mode != MODE_TREE
            && !e.skip_worktree()
            && !e.assume_unchanged()
            && !e.intent_to_add()
            && !e.fsmonitor_valid()
    })
}

fn has_stashable_changes(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    options: &StashPushOptions,
) -> Result<StashableChanges> {
    let found = |unstaged: Option<Vec<DiffEntry>>| Ok(StashableChanges::Found { unstaged });
    let mut index = repo.load_index()?;
    if options.mode == StashMode::StagedOnly {
        if staged_change_paths_for_options(repo, &index, head_tree, options)?.is_empty() {
            return Ok(StashableChanges::Nothing);
        }
        return found(None);
    }
    if !diff_index_to_tree(repo.odb(), &index, Some(head_tree), true)?.is_empty() {
        return found(None);
    }
    let plain = index_is_plain_for_stash(&index);
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
        return found(plain.then_some(unstaged));
    }
    if !include_untracked {
        return Ok(StashableChanges::Nothing);
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
            clean_unit_dirs: None,
        },
        &worktree_rules,
        None,
    )?;
    if untracked.is_empty() {
        return Ok(StashableChanges::Nothing);
    }
    found(plain.then_some(unstaged))
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
            clean_unit_dirs: None,
        },
        &worktree_rules,
        None,
    )?;
    Ok(untracked
        .into_iter()
        .filter(|path| !path_in_nested_git_worktree(work_tree, path))
        .collect())
}

/// Stage every unstaged worktree change into `index`.
///
/// `precomputed` is the index-to-worktree diff of an index with the same entries and stat data
/// as `index`; when absent the diff is computed here.
fn apply_worktree_diff_to_index(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    precomputed: Option<Vec<DiffEntry>>,
    scope: &mut ProgressScope<'_>,
) -> Result<()> {
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    let rules_arc = std::sync::Arc::new(std::sync::Mutex::new(worktree_rules));
    let unstaged = match precomputed {
        Some(unstaged) => unstaged,
        None => {
            let diff_opts = DiffIndexToWorktreeOptions {
                index_mtime,
                ignore_submodule_untracked: true,
                repository_git_dir: Some(repo.git_dir().to_path_buf()),
                config: repo.config().ok(),
                worktree_rules: Some(std::sync::Arc::clone(&rules_arc)),
                ..Default::default()
            };
            diff_index_to_worktree_with_options(repo.odb(), index, work_tree, diff_opts)?.0
        }
    };
    let mut worktree_rules = rules_arc
        .lock()
        .map_err(|e| Error::Message(format!("worktree rules lock poisoned: {e}")))?;
    let precompose = crate::precompose_config::effective_core_precomposeunicode_with_config(
        Some(repo.git_dir()),
        Some(&worktree_rules.config_arc()),
    );
    scope
        .sink()
        .start(ProgressPhase::StagingFiles, Some(unstaged.len() as u64));
    for entry in unstaged {
        scope.poll()?;
        scope.sink().inc(1);
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
    scope.sink().finish();
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
                index.invalidate_cache_tree_for_path(rel_path.as_bytes());
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
        .map_err(|e| materialize_stash_blob_err(e, &index_relpath))?
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
    write_commit_object(repo, &data, STASH_COMMIT_SIGNING)
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

/// Stash commits are local bookkeeping and are never signed, whatever `commit.gpgsign` says
/// (Git creates them unsigned too).
const STASH_COMMIT_SIGNING: Option<bool> = Some(false);

struct BuiltStash {
    w_commit: ObjectId,
    w_tree: ObjectId,
    i_tree: ObjectId,
    message: String,
    untracked_paths: Vec<String>,
    partial: Option<ResolvedSelection>,
    push_pathspecs: Vec<String>,
    staged_only_paths: Vec<String>,
    mode: StashMode,
}

fn selection_is_active(options: &StashPushOptions) -> bool {
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
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    if staged_paths.is_empty() {
        return Ok(());
    }
    let head_entries = flatten_tree_full_for_repo(repo, head_tree, "")?;
    let index = repo.load_index()?;
    let worktree_rules = WorktreeRules::from_repository(repo, &index)?;
    progress.start(
        ProgressPhase::CheckingOutFiles,
        Some(staged_paths.len() as u64),
    );
    for path in staged_paths {
        poll_cancel(progress, cancel)?;
        progress.inc(1);
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
    progress.finish();
    Ok(())
}

/// Reset `staged_paths` to `head_tree` in the work tree and index. A cancel stops between
/// files and leaves the index unwritten.
fn apply_staged_only_reset(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    staged_paths: &[String],
    lock: &mut IndexLock,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    if staged_paths.is_empty() {
        return Ok(());
    }
    let head_entries = flatten_tree_full_for_repo(repo, head_tree, "")?;
    let mut index = repo.load_index()?;
    let worktree_rules = WorktreeRules::from_repository(repo, &index)?;
    progress.start(
        ProgressPhase::CheckingOutFiles,
        Some(staged_paths.len() as u64),
    );
    for path in staged_paths {
        poll_cancel(progress, cancel)?;
        progress.inc(1);
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
    progress.finish();
    repo.write_index_with_lock(lock, &mut index, false)
}

fn tracked_stash_preview_paths(
    repo: &Repository,
    work_tree: &Path,
    head_tree: &ObjectId,
    options: &StashPushOptions,
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
    Ok(paths.into_iter().collect())
}

/// List paths that would be stashed without writing objects, refs, or worktree state.
///
/// Returns `None` when there is nothing to save.
///
/// # Errors
///
/// Propagates validation and diff failures.
pub(crate) fn preview_stash(
    repo: &Repository,
    options: &StashPushOptions,
) -> Result<Option<StashPreview>> {
    options.validate()?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot stash in a bare repository".into()))?;
    let head = resolve_head_for_repository(repo)?;
    let head_oid = head_commit_oid(&head)?;
    let head_tree = repo.odb().commit_tree(&head_oid)?;

    if let Some(partial) = resolve_partial_stash(repo, options)? {
        if partial.patches.is_empty() {
            return Ok(None);
        }
        let paths: Vec<String> = partial
            .patches
            .into_iter()
            .filter_map(|p| p.new_path.or(p.old_path))
            .collect();
        return Ok(Some(StashPreview {
            paths,
            untracked: Vec::new(),
            stashed_hunks: partial.selected_hunks,
            stashed_lines: partial.selected_lines,
        }));
    }

    options.validate_staged_worktree(repo, work_tree, &head_tree)?;
    if matches!(
        has_stashable_changes(repo, work_tree, &head_tree, options)?,
        StashableChanges::Nothing
    ) {
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
    Ok(Some(StashPreview {
        paths,
        untracked,
        stashed_hunks: BTreeMap::new(),
        stashed_lines: BTreeMap::new(),
    }))
}

fn stash_push_pathspecs_active(options: &StashPushOptions) -> bool {
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
    options: &StashPushOptions,
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
    options: &StashPushOptions,
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
    scope: &mut ProgressScope<'_>,
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
    scope
        .sink()
        .start(ProgressPhase::StagingFiles, Some(unstaged.len() as u64));
    for entry in unstaged {
        scope.poll()?;
        scope.sink().inc(1);
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
    scope.sink().finish();
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
    lock: &mut IndexLock,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
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
    restore_paths_with_lock(repo, &opts, lock, repo.load_index()?, progress, cancel)?;
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
        DirectoryRenameMode::Off,
    )?;
    if output.index.entries.iter().any(|entry| entry.stage() != 0) {
        return Err(Error::StashIndexConflict);
    }
    Ok(Some(output.result_tree))
}

fn resolve_partial_stash(
    repo: &Repository,
    options: &StashPushOptions,
) -> Result<Option<ResolvedSelection>> {
    if !selection_is_active(options) {
        return Ok(None);
    }
    let Some(sel) = options.selection.as_ref() else {
        return Ok(None);
    };
    Ok(Some(resolve_unstaged_selection(
        repo,
        sel,
        &options.pathspecs,
    )?))
}

fn build_stash_commit(
    repo: &Repository,
    work_tree: &Path,
    options: &StashPushOptions,
    scope: &mut ProgressScope<'_>,
) -> Result<Option<BuiltStash>> {
    options.validate()?;
    let head = resolve_head_for_repository(repo)?;
    let head_oid = head_commit_oid(&head)?;
    let head_tree = repo.odb().commit_tree(&head_oid)?;
    options.validate_staged_worktree(repo, work_tree, &head_tree)?;
    let identity = options.identity.header_line();

    let pathspec_push = stash_push_pathspecs_active(options);
    if pathspec_push {
        validate_stash_push_pathspecs(repo, work_tree, options)?;
    }

    let partial = resolve_partial_stash(repo, options)?;
    let mut reusable_unstaged = None;
    if let Some(ref partial) = partial {
        if partial.patches.is_empty() {
            return Ok(None);
        }
    } else if pathspec_push {
        if !has_stashable_changes_for_pathspecs(repo, work_tree, &head_tree, options)? {
            return Ok(None);
        }
    } else {
        match has_stashable_changes(repo, work_tree, &head_tree, options)? {
            StashableChanges::Nothing => return Ok(None),
            StashableChanges::Found { unstaged } => reusable_unstaged = unstaged,
        }
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
        author: identity.clone(),
        committer: identity.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: index_msg,
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let i_commit = write_commit_object(repo, &i_commit_data, STASH_COMMIT_SIGNING)?;

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
            &identity,
        )?)
    };

    let w_tree = if options.mode == StashMode::StagedOnly {
        i_tree
    } else {
        // Seeding stat data from the index I was written from lets the worktree diff below hash
        // only files whose stat changed, and the primed cache-tree limits the W tree write to
        // the directories those files invalidate.
        let mut w_index = index_from_tree_with_seeded_stats(repo, &i_tree, &index)?;
        if let Some(ref partial) = partial {
            apply_patch_to_index(
                repo,
                &mut w_index,
                &partial.patches,
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
                scope,
            )?;
        } else {
            apply_worktree_diff_to_index(repo, work_tree, &mut w_index, reusable_unstaged, scope)?;
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
        author: identity.clone(),
        committer: identity.clone(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: w_message.clone(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let w_commit = write_commit_object(repo, &w_commit_data, STASH_COMMIT_SIGNING)?;

    Ok(Some(BuiltStash {
        w_commit,
        w_tree,
        i_tree,
        message: w_message.trim_end().to_owned(),
        untracked_paths,
        partial,
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
/// The stash commits are never signed, even when `commit.gpgsign` is set (Git
/// creates them unsigned too).
///
/// # Errors
///
/// Returns [`Error::StashNoInitialCommit`] when `HEAD` does not resolve to a commit,
/// or propagates index, diff, and object-database failures.
pub(crate) fn create_stash(
    repo: &Repository,
    options: &StashPushOptions,
) -> Result<Option<ObjectId>> {
    let work_tree = repo.work_tree().ok_or(Error::StashBareRepository)?;
    let mut progress = NullProgress;
    let mut scope = ProgressScope::new(&mut progress, &NeverCancel);
    Ok(build_stash_commit(repo, work_tree, options, &mut scope)?.map(|b| b.w_commit))
}

/// Point `refs/stash` at `oid` and append a reflog entry with `message`.
///
/// # Errors
///
/// Propagates ref and reflog update failures.
pub(crate) fn store_stash(
    repo: &Repository,
    oid: ObjectId,
    message: &str,
    identity: &str,
) -> Result<()> {
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

/// Result of a successful [`RepositoryStash::push`](crate::repo_handles::RepositoryStash::push).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashPushResult {
    /// The W commit object id stored on `refs/stash`.
    pub oid: ObjectId,
    /// Reflog-style message stored on `refs/stash`.
    pub message: String,
    /// Tree recorded on the W commit.
    pub w_tree: ObjectId,
    /// For a partial push ([`StashPushOptions::selection`]), the hunk ids that were stashed, per
    /// repository-relative path. Empty otherwise.
    pub stashed_hunks: BTreeMap<String, Vec<HunkId>>,
    /// For a partial push, the requested line range labels (`start` or `start-end`) that were
    /// stashed, per path. Empty otherwise.
    pub stashed_lines: BTreeMap<String, Vec<String>>,
}

/// Create a stash entry, store it, and reset the worktree and index to `HEAD`.
///
/// Returns `None` when there is nothing to stash. Untracked paths included in
/// the stash are removed from the worktree after the reset. Like
/// [`create_stash`], the stash commits are written unsigned.
///
/// `index.lock` is held from before the index is read until the reset index is written, so
/// a lock held by another process refuses the push before any object, ref or file is written.
/// Once `refs/stash` is updated, the entry stays there even if the reset fails, so the saved
/// changes remain reachable.
///
/// Hashing work-tree changes reports [`ProgressPhase::StagingFiles`] and the reset reports
/// [`ProgressPhase::CheckingOutFiles`]. A cancel before `refs/stash` is updated leaves the
/// repository as it was (stash objects already written stay unreachable); a cancel during
/// the reset keeps the stored entry, like a failed reset.
///
/// # Errors
///
/// [`Error::LockHeld`] when another process holds `index.lock`, with the repository
/// unchanged; [`Error::Cancelled`] when `cancel` or [`ProgressSink::should_cancel`] fired;
/// otherwise the same as [`create_stash`] and [`store_stash`], plus checkout failures.
pub(crate) fn push_stash(
    repo: &Repository,
    options: &StashPushOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<Option<StashPushResult>> {
    if options.dry_run {
        return Err(Error::StashOptionConflict(
            "stash push does not run with dry_run; use RepositoryStash::preview".into(),
        ));
    }
    let work_tree = repo.work_tree().ok_or(Error::StashBareRepository)?;
    let mut lock = repo.lock_index()?;
    let mut scope = ProgressScope::new(progress, cancel);
    scope.poll()?;
    let Some(built) = build_stash_commit(repo, work_tree, options, &mut scope)? else {
        return Ok(None);
    };
    scope.poll()?;
    store_stash(
        repo,
        built.w_commit,
        &built.message,
        &options.identity.header_line(),
    )?;
    reset_after_stash_push(repo, work_tree, &built, &mut lock, scope.sink(), cancel)?;
    let (stashed_hunks, stashed_lines) = built
        .partial
        .map(|partial| (partial.selected_hunks, partial.selected_lines))
        .unwrap_or_default();
    Ok(Some(StashPushResult {
        oid: built.w_commit,
        message: built.message,
        w_tree: built.w_tree,
        stashed_hunks,
        stashed_lines,
    }))
}

/// Remove the stashed changes from the worktree and index, writing the index through `lock`.
fn reset_after_stash_push(
    repo: &Repository,
    work_tree: &Path,
    built: &BuiltStash,
    lock: &mut IndexLock,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    if !built.untracked_paths.is_empty() {
        remove_stashed_untracked(repo, work_tree, &built.untracked_paths)?;
    }
    if let Some(ref partial) = built.partial {
        return reverse_apply_patches_to_worktree(
            repo,
            &partial.patches,
            &WorktreeReverseApplyOptions { dry_run: false },
            progress,
            cancel,
        )
        .map_err(stash_worktree_step_error);
    }
    let head = resolve_head_for_repository(repo)?;
    let head_oid = head_commit_oid(&head)?;
    let head_tree = repo.odb().commit_tree(&head_oid)?;
    if !built.push_pathspecs.is_empty() {
        let reset_specs = pathspecs_for_push_reset(repo, &head_tree, &built.push_pathspecs)?;
        reset_stash_push_pathspecs_to_head(repo, &head_tree, &reset_specs, lock, progress, cancel)?;
        if built.mode == StashMode::StagedOnly {
            let index = repo.load_index()?;
            let all_staged = staged_change_paths(repo, &index, &head_tree)?;
            let stashed_set: HashSet<&str> =
                built.staged_only_paths.iter().map(String::as_str).collect();
            let worktree_only: Vec<String> = all_staged
                .into_iter()
                .filter(|path| !stashed_set.contains(path.as_str()))
                .collect();
            apply_staged_worktree_only_reset(
                repo,
                work_tree,
                &head_tree,
                &worktree_only,
                progress,
                cancel,
            )?;
        }
        return Ok(());
    }
    match built.mode {
        StashMode::StagedOnly => apply_staged_only_reset(
            repo,
            work_tree,
            &head_tree,
            &built.staged_only_paths,
            lock,
            progress,
            cancel,
        ),
        StashMode::KeepIndex => checkout_between_trees_locked(
            repo,
            &built.w_tree,
            &built.i_tree,
            None,
            CheckoutIndexWrite::Commit(lock),
            progress,
            cancel,
        ),
        StashMode::All => {
            let mut checked_out = repo.load_index()?;
            checkout_between_trees_locked(
                repo,
                &built.w_tree,
                &head_tree,
                Some(&mut checked_out),
                CheckoutIndexWrite::Defer(lock),
                progress,
                cancel,
            )?;
            // The checkout left fresh stat data for every path it rewrote; reuse it so the
            // refresh only hashes files whose stat does not match.
            let mut head_index =
                index_from_tree_with_seeded_worktree_stats(repo, &head_tree, &checked_out)?;
            drop(checked_out);
            repo.write_index_with_lock(lock, &mut head_index, true)
        }
    }
}

fn stash_worktree_step_error(err: ApplyError) -> Error {
    match err {
        ApplyError::Cancelled => Error::Cancelled,
        ApplyError::Failed { path, message } if !path.is_empty() => Error::Message(format!(
            "could not update '{path}' while finishing stash: {message}"
        )),
        other => Error::Message(other.to_string()),
    }
}

fn materialize_stash_blob_err(err: Error, index_relpath: &str) -> Error {
    match err {
        Error::Io(source) => map_worktree_io(index_relpath, WorktreePathOperation::Read, source),
        other => other,
    }
}

/// List stash entries from `refs/stash` reflog (newest first).
///
/// # Errors
///
/// Propagates reflog read failures.
pub(crate) fn list_stashes(repo: &Repository) -> Result<Vec<StashEntry>> {
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
pub(crate) fn drop_stash(repo: &Repository, n: usize) -> Result<()> {
    let store = repo.refs();
    let entries = read_reflog_on_store(store, STASH_REF)?;
    if n >= entries.len() {
        return Err(Error::StashNotFound { n });
    }
    delete_reflog_entries_rechain_on_store(store, STASH_REF, &[n])?;
    let remaining = read_reflog_on_store(store, STASH_REF)?;
    if let Some(top_entry) = remaining.last() {
        update_direct_ref_on_store(store, repo.git_dir(), STASH_REF, &top_entry.new_oid)?;
    } else {
        delete_ref_on_store(store, STASH_REF)?;
    }
    Ok(())
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
pub(crate) fn stash_diff(
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
                let (old_oid, old_mode, status) = if let Some(h) = head_entry {
                    (
                        h.oid,
                        FileMode::new(h.mode),
                        DiffStatus::for_content_change(h.mode, entry.mode),
                    )
                } else {
                    (zero_oid(), FileMode::ABSENT, DiffStatus::Modified)
                };
                diffs[idx] = DiffEntry {
                    status,
                    old_path: Some(entry.path.clone()),
                    new_path: Some(entry.path.clone()),
                    old_mode,
                    new_mode: FileMode::new(entry.mode),
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
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::new(entry.mode),
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
pub(crate) fn stash_branch(repo: &Repository, name: &str, n: usize) -> Result<StashBranchOutcome> {
    validate_branch_short_name(name)
        .map_err(|e| Error::Message(format!("invalid branch name '{name}': {e}")))?;
    let branch_ref = format!("refs/heads/{name}");
    if repo.resolve_ref_oid(&branch_ref).is_ok() {
        return Err(BranchError::AlreadyExists {
            name: name.to_owned(),
        }
        .into());
    }
    if repo.work_tree().is_none() {
        return Err(Error::StashBareRepository);
    }
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
            hooks: crate::hooks::PostOperationHooks::Run,
        },
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )?;

    let applied = apply_stash_entry(
        repo,
        n,
        &StashApplyOptions {
            restore_index: true,
        },
        &mut NullProgress,
        &NeverCancel,
    )?;
    if !applied.has_conflicts() {
        drop_stash(repo, n)?;
    }
    Ok(StashBranchOutcome {
        branch: name.to_owned(),
        base,
        index: n,
        conflicts: applied.has_conflicts(),
        conflict_paths: applied.conflicts.into_iter().map(|c| c.path).collect(),
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
pub(crate) fn clear_stashes(repo: &Repository) -> Result<usize> {
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

/// One filter-free regular file applied from a stash tree blob via bulk checkout.
struct StashCheckoutJob<'a> {
    rel_path: &'a str,
    mode: u32,
    oid: ObjectId,
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

/// Whether the cached stat data of `ie` proves the worktree file still matches the index.
///
/// `false` means "unknown", not "modified": callers must fall back to a content comparison.
/// Racily clean entries (modified within the index file's mtime granularity) are never trusted.
fn index_entry_stat_clean(
    work_tree: &Path,
    ie: &IndexEntry,
    index_mtime: Option<(u32, u32)>,
    stat_policy: crate::index::StatMatchPolicy,
    trust_filemode: bool,
) -> bool {
    if ie.stat_cache_unset() || ie.mode == MODE_GITLINK {
        return false;
    }
    let Ok(rel) = std::str::from_utf8(&ie.path) else {
        return false;
    };
    let Ok(meta) = fs::symlink_metadata(work_tree.join(rel)) else {
        return false;
    };
    if meta.is_dir() || meta.file_type().is_symlink() != (ie.mode == MODE_SYMLINK) {
        return false;
    }
    if trust_filemode && crate::diff::mode_from_metadata(&meta) != ie.mode {
        return false;
    }
    crate::diff::stat_matches_with_policy(ie, &meta, stat_policy)
        && !crate::diff::entry_is_racy_with_policy(ie, index_mtime, stat_policy)
}

/// Refuse to apply when the stash would clobber a locally-modified file.
pub fn check_stash_apply_would_overwrite_local_changes(
    repo: &Repository,
    work_tree: &Path,
    stash_commit: &CommitData,
) -> Result<()> {
    let index = load_stash_index(repo)?;
    let head_at_stash = stash_commit
        .parents
        .first()
        .ok_or(Error::CorruptStash("expected at least 2 parents"))?;
    let head_tree = repo.odb().commit_tree(head_at_stash)?;
    let delta = StashTreeDelta::read(repo, &head_tree, &stash_commit.tree)?;
    check_stash_local_changes(
        repo,
        work_tree,
        &index,
        delta.changes.iter().map(|(path, _)| path.as_slice()),
    )
}

fn check_stash_local_changes<'a>(
    repo: &Repository,
    work_tree: &Path,
    current_index: &Index,
    paths: impl IntoIterator<Item = &'a [u8]>,
) -> Result<()> {
    let config = repo.config()?;
    let stat_policy = crate::index::StatMatchPolicy::from_config(Some(config.as_ref()));
    let trust_filemode = config
        .get_bool("core.filemode")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let index_mtime = crate::diff::index_mtime_for_diff(current_index, None);
    let comparer =
        crate::diff::WorktreeEntryComparer::new(repo.git_dir(), work_tree, Some(config.as_ref()));

    for path in paths {
        let Some(idx_entry) = current_index.get(path, 0) else {
            continue;
        };
        if idx_entry.mode == MODE_GITLINK {
            continue;
        }
        if index_entry_stat_clean(
            work_tree,
            idx_entry,
            index_mtime,
            stat_policy,
            trust_filemode,
        ) {
            continue;
        }
        if comparer.differs(repo.odb(), idx_entry, true, None)? {
            return Err(Error::StashWouldOverwriteLocalChanges {
                paths: crate::repo_relpath::storage_string(path),
            });
        }
    }
    Ok(())
}

/// Worktree paths that would block materializing the stash untracked-files parent.
fn collect_worktree_paths_blocking_stash_untracked_restore(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
) -> Result<Vec<String>> {
    let mut paths = HashSet::new();
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
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    let (untracked, ignored, _) = collect_untracked_and_ignored_inner(
        repo,
        index,
        work_tree,
        &[],
        UntrackedScan {
            ignored_mode: IgnoredMode::Traditional,
            show_all: true,
            sort_paths: true,
            use_untracked_cache: false,
            cache_miss: UntrackedCacheMissAction::RefreshFull,
            clean_unit_dirs: None,
        },
        &worktree_rules,
        None,
    )?;
    paths.extend(untracked);
    paths.extend(ignored);
    let mut sorted: Vec<String> = paths.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

/// Refuse when restoring the stash untracked-files parent would clobber an existing worktree path.
///
/// Must run before any worktree or index mutations in [`apply_stash_oid`].
///
/// # Errors
///
/// Returns [`Error::StashCouldNotRestoreUntracked`] when a path from the untracked parent
/// collides with tracked, untracked, or ignored content already on disk.
fn check_stash_untracked_restore(
    repo: &Repository,
    work_tree: &Path,
    stash_commit: &CommitData,
    index: &mut Index,
    wt_deletions: &BTreeSet<Vec<u8>>,
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
    let ut_file_paths: HashSet<String> = ut_entries
        .iter()
        .filter(|e| e.mode != MODE_GITLINK && e.mode != MODE_TREE)
        .map(|e| e.path.clone())
        .collect();
    let existing_paths: Vec<String> =
        collect_worktree_paths_blocking_stash_untracked_restore(repo, work_tree, index)?
            .into_iter()
            .filter(|path| {
                for ut_file in &ut_file_paths {
                    if path != ut_file
                        && path.starts_with(&format!("{ut_file}/"))
                        && wt_deletions.contains(path.as_bytes())
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
            old_mode: FileMode::ABSENT,
            new_mode: FileMode::new(entry.mode),
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
/// Mirrors `git stash apply`: three-way-merges changed files when `HEAD` has moved since the
/// stash was created, restores the index from the stash index parent when
/// [`StashApplyOptions::restore_index`] is set (otherwise tracks current `HEAD` at touched
/// paths), and materializes any untracked-files parent.
///
/// # Errors
///
/// Returns [`Error::StashBareRepository`] without a work tree, [`Error::CorruptStash`] for a
/// malformed stash commit, and local-change, untracked-overwrite, or index write failures.
pub(crate) fn apply_stash_oid(
    repo: &Repository,
    stash_oid: &ObjectId,
    options: &StashApplyOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<StashApplyOutcome> {
    let work_tree = repo.work_tree().ok_or(Error::StashBareRepository)?;
    apply_stash_commit(
        repo,
        work_tree,
        stash_oid,
        options.restore_index,
        progress,
        cancel,
    )
}

/// Result of [`RepositoryStash::apply`](crate::repo_handles::RepositoryStash::apply),
/// [`RepositoryStash::apply_commit`](crate::repo_handles::RepositoryStash::apply_commit), and
/// [`RepositoryStash::pop`](crate::repo_handles::RepositoryStash::pop).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct StashApplyOutcome {
    /// Paths left with conflict stages (1–3) in the index, in index order, each with the
    /// [`ConflictKind`](crate::operation::ConflictKind) `status` reports for it.
    ///
    /// Built by [`conflicts_from_index`], so paths use
    /// [`crate::repo_relpath::storage_string`] to preserve raw Git path bytes. Empty when the
    /// stash applied cleanly.
    pub conflicts: Vec<Conflict>,
}

impl StashApplyOutcome {
    /// Whether the apply left merge conflicts in the worktree and index.
    ///
    /// After a pop, `true` also means the stash entry was kept.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

/// Apply `stash@{{n}}` (newest-first) onto the current worktree and index without dropping it.
///
/// # Errors
///
/// Returns [`Error::StashNotFound`] when `n` is out of range, plus the [`apply_stash_oid`] errors.
pub(crate) fn apply_stash_entry(
    repo: &Repository,
    n: usize,
    options: &StashApplyOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<StashApplyOutcome> {
    let oid = stash_oid_at(repo, n)?;
    apply_stash_oid(repo, &oid, options, progress, cancel)
}

/// Apply `stash@{{n}}` and drop it when it applied without conflicts.
///
/// # Errors
///
/// Propagates [`apply_stash_entry`] and [`drop_stash`] failures.
pub(crate) fn pop_stash_entry(
    repo: &Repository,
    n: usize,
    options: &StashApplyOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<StashApplyOutcome> {
    let outcome = apply_stash_entry(repo, n, options, progress, cancel)?;
    if !outcome.has_conflicts() {
        drop_stash(repo, n)?;
    }
    Ok(outcome)
}

fn apply_stash_commit(
    repo: &Repository,
    work_tree: &Path,
    stash_oid: &ObjectId,
    restore_index: bool,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<StashApplyOutcome> {
    let stash_commit = repo.odb().read_commit(stash_oid)?;

    if stash_commit.parents.len() < 2 {
        return Err(Error::CorruptStash("expected at least 2 parents"));
    }

    let head_at_stash = &stash_commit.parents[0];
    let index_commit_oid = &stash_commit.parents[1];

    let mut lock = repo.lock_index()?;
    let mut new_index = load_stash_index(repo)?;
    let head_at_stash_tree = repo.odb().commit_tree(head_at_stash)?;
    let idx_tree = repo.odb().commit_tree(index_commit_oid)?;
    let merged_index_tree = if restore_index {
        let current_index_tree = write_stash_tree_from_index(repo, &mut new_index)?;
        merge_stash_index_trees(repo, &head_at_stash_tree, &current_index_tree, &idx_tree)?
    } else {
        None
    };

    let StashTreeDelta {
        mut base,
        changes: wt_changes,
    } = StashTreeDelta::read(repo, &head_at_stash_tree, &stash_commit.tree)?;
    check_stash_local_changes(
        repo,
        work_tree,
        &new_index,
        wt_changes.iter().map(|(p, _)| p.as_slice()),
    )?;
    let deletions = wt_changes
        .iter()
        .filter(|(_, e)| e.is_none())
        .map(|(p, _)| p.clone())
        .collect();
    check_stash_untracked_restore(repo, work_tree, &stash_commit, &mut new_index, &deletions)?;
    drop(deletions);

    // Include the index delta even when a staged addition was removed again in the worktree.
    let index_delta = StashTreeDelta::read(repo, &head_at_stash_tree, &idx_tree)?;
    base.extend(index_delta.base);
    base.sort_by(|a, b| a.path.cmp(&b.path));
    base.dedup_by(|a, b| a.path == b.path);
    let base_tree_entries = base;
    let idx_tree_loaded: Vec<_> = if idx_tree == head_at_stash_tree {
        Vec::new()
    } else {
        let mut entries: BTreeMap<_, _> = base_tree_entries
            .iter()
            .map(|e| (e.path.clone(), e.clone()))
            .collect();
        for (path, entry) in index_delta.changes {
            if let Some(entry) = entry {
                entries.insert(path, entry);
            } else {
                entries.remove(&path);
            }
        }
        entries.into_values().collect()
    };
    let idx_tree_entries = if idx_tree == head_at_stash_tree {
        base_tree_entries.as_slice()
    } else {
        idx_tree_loaded.as_slice()
    };
    // Determine if HEAD has moved since the stash was created
    let current_head = resolve_head_for_repository(repo)?;
    let current_head_oid = current_head.oid().copied();
    let head_moved = current_head_oid.as_ref() != Some(head_at_stash);

    // Current HEAD tree (three-way merge when HEAD moved; index reset without `--index`).
    let current_head_loaded: Vec<StashTreeEntry>;
    let current_head_entries: &[StashTreeEntry] = if head_moved {
        let Some(head_oid) = current_head_oid.as_ref() else {
            return Err(Error::StashMissingHead);
        };
        let head_tree = repo.odb().commit_tree(head_oid)?;
        let affected: BTreeSet<_> = wt_changes
            .iter()
            .map(|(p, _)| p.as_slice())
            .chain(idx_tree_entries.iter().map(|e| e.path.as_slice()))
            .collect();
        current_head_loaded = affected
            .into_iter()
            .map(|path| stash_tree_entry_at_path(repo, head_tree, path))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        &current_head_loaded
    } else {
        current_head_loaded = Vec::new();
        &base_tree_entries
    };

    let mut has_conflicts = false;
    let worktree_rules = WorktreeRules::from_repository(repo, &new_index)?;
    poll_cancel(progress, cancel)?;

    // First pass: process deletions (None entries) before additions to avoid
    // type conflicts (e.g., trying to write a file where a directory exists).
    for (path, change) in &wt_changes {
        if change.is_some() {
            continue;
        }
        poll_cancel(progress, cancel)?;
        let file_path = crate::index::worktree_path_from_index_rel(work_tree, path);
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
    let mut prepared_dirs: HashSet<std::path::PathBuf> = HashSet::new();
    let mut parallel_writes: Vec<StashCheckoutJob<'_>> = Vec::new();
    for (path, change) in &wt_changes {
        // Deletions all ran in the first pass. Pruning directories here could remove the
        // parent of a regular file still queued for bulk checkout.
        let Some(entry) = change else {
            continue;
        };
        poll_cancel(progress, cancel)?;
        let file_path = crate::index::worktree_path_from_index_rel(work_tree, path);
        if let Some(parent) = file_path.parent() {
            // Entries of one tree never use another entry's path as a directory, so a
            // parent prepared once stays a directory for the rest of this pass.
            if !prepared_dirs.contains(parent) {
                // If a component of the parent is a file, remove it first
                let mut cur = work_tree.to_path_buf();
                if let Ok(rel) = parent.strip_prefix(work_tree) {
                    for comp in rel.components() {
                        cur.push(comp);
                        if cur.exists() && !cur.is_dir() {
                            let _ = fs::remove_file(&cur);
                        }
                    }
                }
                fs::create_dir_all(parent)?;
                prepared_dirs.insert(parent.to_path_buf());
            }
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

        if !head_moved && entry.mode != MODE_SYMLINK {
            let file_attrs = worktree_rules.file_attrs(&String::from_utf8_lossy(path), false);
            // The bulk driver matches attributes against its stored path strings. Keep
            // encoded names on this writer, which matches against the original name.
            if let Ok(rel_path) = std::str::from_utf8(path) {
                if !rel_path.starts_with(crate::repo_relpath::STORAGE_PREFIX)
                    && is_eligible_for_parallel_checkout(entry.mode, &file_attrs)
                {
                    parallel_writes.push(StashCheckoutJob {
                        rel_path,
                        mode: entry.mode,
                        oid: entry.oid,
                    });
                    continue;
                }
            }
        }

        let stash_blob = repo.odb().read(&entry.oid)?;

        if entry.mode == MODE_SYMLINK {
            let target =
                String::from_utf8(stash_blob.data).map_err(|_| Error::StashSymlinkNotUtf8)?;
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
            let base_content = StashTreeEntry::lookup(&base_tree_entries, path)
                .and_then(|e| repo.odb().read(&e.oid).ok())
                .map(|o| o.data)
                .unwrap_or_default();
            let ours_content = StashTreeEntry::lookup(current_head_entries, path)
                .and_then(|e| repo.odb().read(&e.oid).ok())
                .map(|o| o.data)
                .unwrap_or_default();
            let theirs_content = stash_blob.data;

            // If ours == base, no conflict (only stash changed this file)
            if ours_content == base_content {
                write_smudged_regular_file_replacing_symlink(
                    repo,
                    &worktree_rules,
                    &String::from_utf8_lossy(path),
                    &file_path,
                    entry.mode,
                    &theirs_content,
                    &entry.oid,
                )?;
            } else if ours_content == theirs_content {
                // Both changed the same way, no conflict
                let smudge_oid = StashTreeEntry::lookup(current_head_entries, path)
                    .map(|e| e.oid)
                    .unwrap_or(entry.oid);
                write_smudged_regular_file_replacing_symlink(
                    repo,
                    &worktree_rules,
                    &String::from_utf8_lossy(path),
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
                    &String::from_utf8_lossy(path),
                    entry.mode,
                    &output.content,
                    &marker_oid,
                )?;
                write_regular_file_replacing_symlink(&file_path, &worktree_bytes)?;
                if output.conflicts > 0 {
                    has_conflicts = true;
                    // Write conflict stages to index
                    let path_bytes = path.as_slice();
                    let original_mode = new_index
                        .get(path_bytes, 0)
                        .map(|e| e.mode)
                        .unwrap_or(crate::index::MODE_REGULAR);
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
                    if let Some(base_entry) = StashTreeEntry::lookup(&base_tree_entries, path) {
                        add_stage_entry(
                            &mut new_index,
                            path_bytes,
                            &base_entry.oid,
                            base_entry.mode,
                            1,
                        );
                    }
                    if let Some(ours_entry) = StashTreeEntry::lookup(current_head_entries, path) {
                        add_stage_entry(
                            &mut new_index,
                            path_bytes,
                            &ours_entry.oid,
                            original_mode,
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
                &String::from_utf8_lossy(path),
                &file_path,
                entry.mode,
                &stash_blob.data,
                &entry.oid,
            )?;
        }
    }

    if !parallel_writes.is_empty() {
        let checkout_changes: Vec<DiffEntry> = parallel_writes
            .iter()
            .map(|job| {
                let base = StashTreeEntry::lookup(&base_tree_entries, job.rel_path.as_bytes());
                DiffEntry {
                    status: DiffStatus::Modified,
                    old_path: Some(crate::repo_relpath::storage_string(job.rel_path.as_bytes())),
                    new_path: Some(crate::repo_relpath::storage_string(job.rel_path.as_bytes())),
                    old_mode: FileMode::new(
                        base.map(|e| e.mode).unwrap_or(crate::index::MODE_REGULAR),
                    ),
                    new_mode: FileMode::new(job.mode),
                    old_oid: base.map(|e| e.oid).unwrap_or_else(zero_oid),
                    new_oid: job.oid,
                    score: None,
                }
            })
            .collect();
        crate::porcelain::checkout::checkout_tree_changes_with_index_opts(
            repo,
            &checkout_changes,
            &mut new_index,
            progress,
            cancel,
            crate::porcelain::checkout::CheckoutApplyOptions {
                parallel_prepare_blobs: true,
                ..crate::porcelain::checkout::CheckoutApplyOptions::default()
            },
            None,
            CheckoutIndexWrite::Defer(&lock),
        )?;
    }
    drop(parallel_writes);
    // Update the index

    if restore_index && !has_conflicts {
        if let Some(merged_tree) = merged_index_tree {
            new_index = index_from_tree_with_worktree_stats(repo, &merged_tree)?;
        } else {
            // Stash index matched current index or HEAD at stash time; replay per-path index parent.
            for base_entry in &base_tree_entries {
                if StashTreeEntry::lookup(idx_tree_entries, &base_entry.path).is_none() {
                    new_index.remove(&base_entry.path);
                }
            }
            for idx_entry in idx_tree_entries {
                let path = &idx_entry.path;
                let base = StashTreeEntry::lookup(&base_tree_entries, path);
                if base.map(|e| (e.oid, e.mode)) != Some((idx_entry.oid, idx_entry.mode)) {
                    let path_bytes = path.as_slice();
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
                if let Some(ie) = new_index.get_mut(path, 0) {
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
        let mut touched: BTreeSet<Vec<u8>> = wt_changes.iter().map(|(p, _)| p.clone()).collect();
        for entry in idx_tree_entries {
            if StashTreeEntry::lookup(&base_tree_entries, &entry.path).is_none() {
                touched.insert(entry.path.clone());
            }
        }
        // Unmerged paths only come from the starting index or the conflict pass above, and the
        // loop below never adds or removes conflict stages, so one snapshot covers every path.
        let unmerged: HashSet<Vec<u8>> = new_index
            .entries
            .iter()
            .filter(|e| e.stage() > 0)
            .map(|e| e.path.clone())
            .collect();
        for path in &touched {
            if let Some(te) = StashTreeEntry::lookup(current_head_entries, path) {
                let path_bytes = path.as_slice();
                let size = if te.mode == MODE_SYMLINK || te.mode == MODE_GITLINK {
                    0u32
                } else {
                    repo.odb().read_info(&te.oid)?.size as u32
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
                if !unmerged.contains(path_bytes) {
                    new_index.stage_file(new_entry);
                }
            } else {
                let path_bytes = path.as_slice();
                if unmerged.contains(path_bytes) {
                    continue;
                }
                if let Some(ie) = StashTreeEntry::lookup(idx_tree_entries, path) {
                    let had_staged = match StashTreeEntry::lookup(&base_tree_entries, path) {
                        Some(b) => b.oid != ie.oid || b.mode != ie.mode,
                        None => true,
                    };
                    if had_staged {
                        let size = if ie.mode == MODE_SYMLINK || ie.mode == MODE_GITLINK {
                            0u32
                        } else {
                            repo.odb().read_info(&ie.oid)?.size as u32
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
        if restore_index || head_moved {
            refresh_index_stats_content_verified(repo, work_tree, &mut new_index)?;
        } else {
            // HEAD is unchanged and the index was not restored, so a touched path's index entry
            // can only match the worktree when it holds the very blob just written there (a newly
            // added file whose staged and worktree content agree). Every other touched entry is
            // known to differ, and untouched entries keep their stat data.
            let mut written = Index::new();
            for (path, change) in &wt_changes {
                let Some(stashed) = change else {
                    continue;
                };
                if let Some(ie) = new_index.get(path, 0) {
                    if ie.oid == stashed.oid && ie.mode == stashed.mode {
                        written.push_entry_unsorted(ie.clone());
                    }
                }
            }
            if !written.entries.is_empty() {
                written.sort();
                written.source_mtime = new_index.source_mtime;
                refresh_index_stats_content_verified(repo, work_tree, &mut written)?;
                for refreshed in written.entries {
                    if let Some(ie) = new_index.get_mut(&refreshed.path, 0) {
                        *ie = refreshed;
                    }
                }
            }
        }
    }
    let conflicts = if has_conflicts {
        conflicts_from_index(&new_index)
    } else {
        Vec::new()
    };
    drop(wt_changes);
    drop(base_tree_entries);
    drop(idx_tree_loaded);
    drop(current_head_loaded);
    repo.write_index_with_lock(&mut lock, &mut new_index, false)
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

    Ok(StashApplyOutcome { conflicts })
}

/// Refresh cached stat data for stage-0 entries whose worktree content matches their blob.
fn refresh_index_stats_content_verified(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
) -> Result<()> {
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    crate::diff::refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        index,
        work_tree,
        None,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )?;
    Ok(())
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
pub(crate) fn drop_stash_commit(repo: &Repository, stash_oid: &ObjectId) -> Result<bool> {
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
pub(crate) fn push_operation_autostash(
    repo: &Repository,
    identity: &str,
) -> Result<Option<ObjectId>> {
    Ok(push_stash(
        repo,
        &StashPushOptions {
            message: Some("autostash".to_owned()),
            include_untracked: true,
            identity: identity.parse()?,
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        },
        &mut NullProgress,
        &NeverCancel,
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
pub(crate) fn apply_operation_autostash(repo: &Repository, stash_oid: &ObjectId) -> Result<bool> {
    let conflicts = apply_stash_oid(
        repo,
        stash_oid,
        &StashApplyOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )?
    .has_conflicts();
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
pub(crate) fn restore_operation_autostash(repo: &Repository, stash_oid: &ObjectId) -> Result<()> {
    let conflicts = apply_stash_oid(
        repo,
        stash_oid,
        &StashApplyOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )?
    .has_conflicts();
    if !conflicts {
        let _ = drop_stash_commit(repo, stash_oid)?;
    }
    Ok(())
}

/// Move the merge autostash commit onto `refs/stash` without applying it, matching `git reset`
/// clearing merge state (`save_autostash_ref`).
///
/// # Errors
///
/// Propagates parse, stash store, and I/O failures.
pub(crate) fn save_merge_autostash_ref(repo: &Repository, identity: &str) -> Result<()> {
    let path = repo.git_dir().join("MERGE_AUTOSTASH");
    let Ok(content) = fs::read_to_string(&path) else {
        return Ok(());
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    let stash_oid = ObjectId::from_hex(trimmed)?;
    store_stash(repo, stash_oid, "autostash", identity)?;
    fs::remove_file(&path).map_err(Error::Io)?;
    Ok(())
}

/// Record `stash_oid` as the merge autostash (`MERGE_AUTOSTASH`) of a merge left in progress,
/// so finishing it ([`apply_merge_autostash`]) or aborting it puts the stashed edits back.
///
/// # Errors
///
/// [`Error::Io`] when the file cannot be written.
pub(crate) fn record_merge_autostash(repo: &Repository, stash_oid: &ObjectId) -> Result<()> {
    fs::write(
        repo.git_dir().join("MERGE_AUTOSTASH"),
        format!("{}\n", stash_oid.to_hex()),
    )
    .map_err(Error::Io)
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
pub(crate) fn apply_merge_autostash(repo: &Repository) -> Result<bool> {
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
    let conflicts = apply_stash_oid(
        repo,
        &stash_oid,
        &StashApplyOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )?
    .has_conflicts();
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
        let opts = StashPushOptions {
            message: Some("msg".into()),
            include_untracked: false,
            identity: ident.parse().expect("signature"),
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
        drop_stash(&repo, 0).expect("drop");
        assert!(list_stashes(&repo).expect("list").is_empty());
    }

    #[test]
    fn push_stash_refreshes_index_stat_for_clean_status() {
        use crate::porcelain::status::{status, StatusOptions};
        use crate::progress::{NeverCancel, NullProgress};

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
        let opts = StashPushOptions {
            message: None,
            include_untracked: false,
            identity: "T <t@t> 1 +0000".parse().expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        push_stash(&repo, &opts, &mut NullProgress, &NeverCancel)
            .expect("push")
            .expect("oid");

        let index = repo.load_index().unwrap();
        assert!(index.get(b"f", 0).expect("f").size > 0);
        assert!(index.get(b"g", 0).expect("g").size > 0);
        let st = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
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
        let opts = StashPushOptions {
            message: None,
            include_untracked: false,
            identity: "T <t@t> 1 +0000".parse().expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        assert!(push_stash(&repo, &opts, &mut NullProgress, &NeverCancel)
            .expect("push")
            .is_none());
    }

    #[test]
    fn drop_stash_commit_removes_tip_after_store() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let ident = "T <t@t> 1 +0000";
        fs::write(tmp.path().join("f"), b"dirty\n").unwrap();
        let opts = StashPushOptions {
            message: Some("m".into()),
            include_untracked: false,
            identity: ident.parse().expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        let oid = create_stash(&repo, &opts).expect("create").expect("some");
        store_stash(&repo, oid, "On main: m", ident).expect("store");
        assert_eq!(list_stashes(&repo).expect("list").len(), 1);
        assert!(drop_stash_commit(&repo, &oid).expect("drop"));
        assert!(
            list_stashes(&repo).expect("list").is_empty(),
            "rollback must remove the tip entry"
        );
    }

    #[test]
    fn stash_delta_keeps_only_changed_leaves_and_validates_paths() {
        use crate::objects::{serialize_tree, TreeEntry};
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let blob = repo.odb().write(ObjectKind::Blob, b"base").unwrap();
        let changed = repo.odb().write(ObjectKind::Blob, b"changed").unwrap();
        let tree = |entries: Vec<TreeEntry>| {
            repo.odb()
                .write(ObjectKind::Tree, &serialize_tree(&entries))
                .unwrap()
        };
        let leaf = |name: &str, mode, oid| TreeEntry {
            name: name.as_bytes().to_vec(),
            mode,
            oid,
        };
        let stable = tree(
            (0..20_000)
                .map(|i| leaf(&format!("f{i:05}"), MODE_REGULAR, blob))
                .collect(),
        );
        let base = tree(vec![
            leaf("edited", MODE_REGULAR, blob),
            leaf("removed", MODE_REGULAR, blob),
            leaf("stable", MODE_TREE, stable),
        ]);
        let other = tree(vec![
            leaf("added", MODE_REGULAR, changed),
            leaf("edited", 0o100755, changed),
            leaf("stable", MODE_TREE, stable),
        ]);
        let delta = StashTreeDelta::read(&repo, &base, &other).unwrap();
        assert_eq!(delta.base.len(), 2);
        assert_eq!(
            delta
                .changes
                .iter()
                .map(|(p, _)| p.as_slice())
                .collect::<Vec<_>>(),
            [b"added".as_slice(), b"edited", b"removed"]
        );
        assert_eq!(delta.changes[1].1.as_ref().unwrap().mode, 0o100755);
        assert!(delta.changes[2].1.is_none());
        assert!(stash_tree_entry_at_path(&repo, base, b"removed/child")
            .unwrap()
            .is_none());
        assert!(stash_tree_entry_at_path(&repo, base, b"stable")
            .unwrap()
            .is_none());
        assert_eq!(
            stash_tree_entry_at_path(&repo, base, b"stable/f19999")
                .unwrap()
                .unwrap()
                .oid,
            blob
        );
        let bad = tree(vec![leaf(".git", MODE_REGULAR, changed)]);
        assert!(StashTreeDelta::read(&repo, &base, &bad).is_err());
    }

    #[test]
    fn stash_delta_decodes_paths_before_validation_and_lookup() {
        use crate::index::MODE_REGULAR;
        use crate::objects::{serialize_tree, TreeEntry};

        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let blob = repo.odb().write(ObjectKind::Blob, b"base").unwrap();
        let tree = |name: &[u8], mode, oid| {
            repo.odb()
                .write(
                    ObjectKind::Tree,
                    &serialize_tree(&[TreeEntry {
                        name: name.to_vec(),
                        mode,
                        oid,
                    }]),
                )
                .unwrap()
        };
        let empty = repo.odb().write(ObjectKind::Tree, b"").unwrap();
        for path in [b"grit+pathhex:6162".as_slice(), b"non-utf8-\xff"] {
            let other = tree(path, MODE_REGULAR, blob);
            let delta = StashTreeDelta::read(&repo, &empty, &other).unwrap();
            assert_eq!(delta.changes[0].0, path);
            assert_eq!(
                stash_tree_entry_at_path(&repo, other, path)
                    .unwrap()
                    .unwrap()
                    .oid,
                blob
            );
        }
        let unsafe_child = tree(b".git", MODE_REGULAR, blob);
        let other = tree(b"non-utf8-\xff", MODE_TREE, unsafe_child);
        assert!(StashTreeDelta::read(&repo, &empty, &other).is_err());
    }

    #[test]
    fn check_stash_untracked_restore_rejects_collision() {
        let tmp = TempDir::new().unwrap();
        let repo = init_repo(tmp.path());
        let ident = "T <t@t> 1 +0000";
        fs::write(tmp.path().join("u.txt"), b"stashed\n").unwrap();
        let opts = StashPushOptions {
            message: Some("m".into()),
            include_untracked: true,
            identity: ident.parse().expect("signature"),
            selection: None,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        };
        push_stash(&repo, &opts, &mut NullProgress, &NeverCancel)
            .expect("push")
            .expect("oid");
        fs::write(tmp.path().join("u.txt"), b"precious\n").unwrap();
        let oid = list_stashes(&repo).expect("list")[0].oid;
        let err = super::apply_stash_oid(
            &repo,
            &oid,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
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
    use crate::porcelain::hunk_select::{HunkSelection, LineRange};
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_hunks_dry_run_does_not_mutate_repo() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        let selected_ids = selection.ids.clone();
        let before = fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        let preview = preview_stash(
            &repo,
            &StashPushOptions {
                message: Some("preview".into()),
                include_untracked: false,
                identity: IDENT.parse().expect("signature"),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: true,
                mode: StashMode::default(),
            },
        )
        .expect("preview")
        .expect("paths");
        assert!(preview.paths.iter().any(|p| p == "f.txt"));
        assert_eq!(
            preview.stashed_hunks,
            BTreeMap::from([("f.txt".to_string(), selected_ids)])
        );
        assert!(preview.stashed_lines.is_empty());
        assert_eq!(list_stashes(&repo).expect("list").len(), 0);
        assert_eq!(
            fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
            before,
            "dry-run must not change worktree"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_hunks_stashes_only_selected() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        let selected_ids = selection.ids.clone();
        let pushed = push_stash(
            &repo,
            &StashPushOptions {
                message: Some("partial".into()),
                include_untracked: false,
                identity: IDENT.parse().expect("signature"),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("push")
        .expect("oid");
        assert_eq!(
            pushed.stashed_hunks,
            BTreeMap::from([("f.txt".to_string(), selected_ids)])
        );
        assert!(pushed.stashed_lines.is_empty());
        let w_tree = stash_w_tree(&repo, &pushed.oid);
        let blob = tree_blob(&repo, &w_tree, "f.txt");
        let text = String::from_utf8_lossy(&blob);
        assert!(text.contains("LINE5"), "stashed hunk in W tree: {text}");
        assert!(
            !text.contains("LINE18"),
            "unselected hunk must not be in stash tree: {text}"
        );
        git(tmp.path(), &["fsck", "--strict"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_hunks_worktree_keeps_rest() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        push_stash(
            &repo,
            &StashPushOptions {
                message: None,
                include_untracked: false,
                identity: IDENT.parse().expect("signature"),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
            &mut NullProgress,
            &NeverCancel,
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_hunks_roundtrip_with_git_stash_apply() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        push_stash(
            &repo,
            &StashPushOptions {
                message: None,
                include_untracked: false,
                identity: IDENT.parse().expect("signature"),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
            &mut NullProgress,
            &NeverCancel,
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_hunks_pop_restores() {
        let (tmp, repo, selection) = setup_two_hunk_repo();
        push_stash(
            &repo,
            &StashPushOptions {
                message: None,
                include_untracked: false,
                identity: IDENT.parse().expect("signature"),
                selection: Some(selection),
                pathspecs: Vec::new(),
                dry_run: false,
                mode: StashMode::default(),
            },
            &mut NullProgress,
            &NeverCancel,
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

    fn options_with(selection: Option<HunkSelection>, include_untracked: bool) -> StashPushOptions {
        StashPushOptions {
            message: None,
            include_untracked,
            identity: IDENT.parse().expect("signature"),
            selection,
            pathspecs: Vec::new(),
            dry_run: false,
            mode: StashMode::default(),
        }
    }

    #[test]
    fn validate_rejects_include_untracked_with_selection() {
        let selection = HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 1,
                end: 1,
            }],
        };
        let err = options_with(Some(selection), true)
            .validate()
            .expect_err("conflict");
        assert!(
            matches!(err, Error::StashOptionConflict(ref msg) if msg.contains("--include-untracked")),
            "got {err:?}"
        );
    }

    #[test]
    fn validate_allows_include_untracked_with_empty_selection() {
        options_with(Some(HunkSelection::default()), true)
            .validate()
            .expect("an empty selection is a full stash");
        options_with(None, true).validate().expect("no selection");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_lines_reports_line_labels() {
        let (tmp, repo, _) = setup_two_hunk_repo();
        let selection = HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 6,
                end: 6,
            }],
        };
        let preview = preview_stash(&repo, &options_with(Some(selection.clone()), false))
            .expect("preview")
            .expect("something to stash");
        let expected_lines = BTreeMap::from([("f.txt".to_string(), vec!["6".to_string()])]);
        assert_eq!(preview.stashed_lines, expected_lines);
        assert!(preview.stashed_hunks.is_empty());

        let pushed = push_stash(
            &repo,
            &options_with(Some(selection), false),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("push")
        .expect("stashed");
        assert_eq!(pushed.stashed_lines, expected_lines);
        assert!(pushed.stashed_hunks.is_empty());
        let on_disk = fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert!(!on_disk.contains("LINE5") && on_disk.contains("LINE18"));
        git(tmp.path(), &["fsck", "--strict"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn stash_push_without_selection_reports_no_hunks() {
        let (_tmp, repo, _) = setup_two_hunk_repo();
        let preview = preview_stash(&repo, &options_with(None, false))
            .expect("preview")
            .expect("something to stash");
        assert!(preview.stashed_hunks.is_empty() && preview.stashed_lines.is_empty());
        let pushed = push_stash(
            &repo,
            &options_with(None, false),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("push")
        .expect("stashed");
        assert!(pushed.stashed_hunks.is_empty() && pushed.stashed_lines.is_empty());
    }
}
