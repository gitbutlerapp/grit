//! `git checkout` worktree-apply primitives.
//!
//! `checkout` is a large worktree mutator. Its CLI shell
//! (`grit/src/commands/checkout.rs`) still owns argv/clap parsing,
//! branch-switch messaging, hook dispatch, progress-to-stderr, and the
//! detached-HEAD advice text. This module holds the **pure worktree-apply
//! primitives** that shell calls into: writing a blob's bytes to a working-tree
//! path (handling symlinks, executable bits, and parent-directory
//! preparation), removing now-empty parent directories, and the simple glob
//! matcher used to resolve interactive-patch path filters.
//!
//! These functions compute and apply worktree changes from index/object data
//! and make no presentation decisions — no colour, pager, tty, or stdout.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::config::ConfigSet;
use crate::crlf::{self, ConversionConfig};
use crate::diff::{
    diff_tree_path_change_count, diff_trees, format_mode, zero_oid, DiffEntry, DiffStatus,
};
use crate::error::{Error, Result};
use crate::filter_process::{DelayedCheckoutError, DelayedProcessCheckout, FilterSmudgeMeta};
use crate::index::{
    entry_from_metadata, entry_from_stat, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK,
    MODE_REGULAR, MODE_SYMLINK, MODE_TREE,
};
use crate::objects::{Object, ObjectId, TreeEntry};
use crate::odb::{BoundedRead, Odb};
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp, ParallelOptions};
use crate::path_icase::paths_equal;
use crate::path_population::{validate_tree_walk_entry, validate_worktree_path};
use crate::path_validate::ProtectOptions;
use crate::progress::{
    Cancel, CancelProgress, NeverCancel, NullProgress, ProgressPhase, ProgressSink, ProgressUnit,
};
use crate::repo::Repository;
use crate::sparse_checkout::SparseCheckout;
use crate::worktree::{io_with_path, WorktreePathOperation};
use crate::worktree_rules::WorktreeRules;

#[path = "checkout_delete_rollback.rs"]
mod checkout_delete_rollback;
#[path = "checkout_parallel.rs"]
mod checkout_parallel;
use checkout_delete_rollback::{CheckoutDeleteGuard, SharedCheckoutDeleteRollback};
pub(crate) use checkout_parallel::is_eligible_for_parallel_checkout;
use checkout_parallel::{
    cancellable_chunks, execute_write_jobs, paths_needing_serial_checkout, CheckoutPlanSlot,
    CheckoutWriteJob, WriteJobOutcome,
};

const PARALLEL_PER_FILE_DELETE_MIN: usize = 32;
const POPULATED_TREE_CHECKOUT_CHUNK: usize = 2048;

/// Remove one tracked file during checkout, surfacing [`Error::WorktreePathIo`] on failure.
fn remove_checkout_worktree_file(
    work_tree: &Path,
    rel_path: &str,
    delete_rollback: &SharedCheckoutDeleteRollback,
) -> Result<()> {
    delete_rollback.spool_remove(work_tree, rel_path)
}

fn remove_checkout_files_parallel_cancellable(
    work_tree: &Path,
    paths: &[String],
    options: ParallelOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    delete_rollback: SharedCheckoutDeleteRollback,
) -> Result<(HashSet<String>, Vec<PathBuf>)> {
    let mut parallel_deleted_paths = HashSet::new();
    let mut deleted_abs_paths = Vec::new();
    let wt = work_tree.to_path_buf();
    let (chunks, options) = cancellable_chunks(paths, options);
    for chunk in chunks {
        crate::progress::poll_cancel(progress, cancel)?;
        let rollback = delete_rollback.clone_handle();
        try_par_map(chunk, options, 0, |rel_path| {
            rollback.spool_remove(&wt, rel_path)
        })
        .map_err(|e: ParallelHashError<Error>| match e {
            ParallelHashError::Task(err) => err,
        })?;
        for path in chunk {
            let abs = checkout_stored_worktree_abs(work_tree, path);
            parallel_deleted_paths.insert(path.clone());
            deleted_abs_paths.push(abs);
            progress.inc(1);
            crate::progress::poll_cancel(progress, cancel)?;
        }
    }
    Ok((parallel_deleted_paths, deleted_abs_paths))
}

fn remove_wholesale_dirs_parallel_cancellable(
    work_tree: &Path,
    dirs: &[String],
    options: ParallelOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<Vec<(String, PathBuf)>> {
    let mut removed = Vec::new();
    if dirs.is_empty() {
        return Ok(removed);
    }
    if dirs.len() < 2 {
        for dir in dirs {
            crate::progress::poll_cancel(progress, cancel)?;
            let abs = work_tree.join(dir);
            if remove_wholesale_dir(&abs) {
                removed.push((dir.clone(), abs));
            }
        }
        return Ok(removed);
    }
    let wt = work_tree.to_path_buf();
    let (chunks, options) = cancellable_chunks(dirs, options);
    for chunk in chunks {
        crate::progress::poll_cancel(progress, cancel)?;
        let removed_chunk = try_par_map(chunk, options, 0, |dir| {
            let abs = wt.join(dir);
            Ok(remove_wholesale_dir(&abs).then_some((dir.clone(), abs)))
        })
        .map_err(|e: ParallelHashError<()>| match e {
            ParallelHashError::Task(()) => {
                Error::PathError("parallel wholesale directory removal failed".into())
            }
        })?;
        removed.extend(removed_chunk.into_iter().flatten());
    }
    Ok(removed)
}

/// One blob entry while walking a tree for populated checkout (no full diff list).
struct PopulatedTreeBlob {
    path: String,
    mode: u32,
    oid: ObjectId,
}

struct PopulatedTreeWalkFrame {
    prefix: String,
    entries: Vec<TreeEntry>,
    index: usize,
}

struct PopulatedTreeWalk<'a> {
    odb: &'a Odb,
    stack: Vec<PopulatedTreeWalkFrame>,
    protect: ProtectOptions,
}

impl<'a> PopulatedTreeWalk<'a> {
    fn new(odb: &'a Odb, tree_oid: &ObjectId, protect: ProtectOptions) -> Result<Self> {
        let entries = (*odb.cached_tree_entries(tree_oid)?).clone();
        Ok(Self {
            odb,
            stack: vec![PopulatedTreeWalkFrame {
                prefix: String::new(),
                entries,
                index: 0,
            }],
            protect,
        })
    }

    fn next_blob(&mut self) -> Result<Option<PopulatedTreeBlob>> {
        while let Some(frame) = self.stack.last_mut() {
            if frame.index >= frame.entries.len() {
                self.stack.pop();
                continue;
            }
            let entry = frame.entries[frame.index].clone();
            frame.index += 1;
            let path = crate::repo_relpath::format_path_stored(&frame.prefix, &entry.name);
            validate_tree_walk_entry(
                frame.prefix.as_bytes(),
                &entry.name,
                entry.mode,
                self.protect,
            )?;
            if entry.mode == MODE_TREE {
                let entries = (*self.odb.cached_tree_entries(&entry.oid)?).clone();
                self.stack.push(PopulatedTreeWalkFrame {
                    prefix: path,
                    entries,
                    index: 0,
                });
                continue;
            }
            return Ok(Some(PopulatedTreeBlob {
                path,
                mode: entry.mode,
                oid: entry.oid,
            }));
        }
        Ok(None)
    }

    fn fill_chunk(&mut self, chunk: &mut Vec<PopulatedTreeBlob>) -> Result<bool> {
        chunk.clear();
        while chunk.len() < POPULATED_TREE_CHECKOUT_CHUNK {
            match self.next_blob()? {
                Some(blob) => chunk.push(blob),
                None => break,
            }
        }
        Ok(!chunk.is_empty())
    }
}

fn tree_contains_dot_gitattributes(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
    protect: ProtectOptions,
) -> Result<bool> {
    let entries = (*odb.cached_tree_entries(tree_oid)?).clone();
    for entry in entries {
        if entry.name == b".gitattributes" && entry.mode != MODE_TREE {
            return Ok(true);
        }
        if entry.mode == MODE_TREE {
            validate_tree_walk_entry(prefix.as_bytes(), &entry.name, entry.mode, protect)?;
            let path = crate::repo_relpath::format_path_stored(prefix, &entry.name);
            if tree_contains_dot_gitattributes(odb, &entry.oid, &path, protect)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Forwards `inc`/`set` to the inner sink but ignores `start`/`finish` (populated-tree chunks).
struct ChunkProgressSink<'a>(&'a mut dyn ProgressSink);

impl ProgressSink for ChunkProgressSink<'_> {
    fn start(&mut self, _phase: ProgressPhase, _total: Option<u64>) {}

    fn start_with_unit(&mut self, _phase: ProgressPhase, _unit: ProgressUnit, _total: Option<u64>) {
    }

    fn inc(&mut self, units: u64) {
        self.0.inc(units);
    }

    fn set(&mut self, current: u64) {
        self.0.set(current);
    }

    fn finish(&mut self) {}
}

/// Populated-tree checkout metadata (case-insensitive path collisions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckoutOutcome {
    /// Stage-0 paths that share a worktree slot when `core.ignorecase` is set.
    pub path_collisions: Vec<String>,
}

fn populated_blob_to_added_diff(blob: &PopulatedTreeBlob) -> DiffEntry {
    DiffEntry {
        status: DiffStatus::Added,
        old_path: None,
        new_path: Some(blob.path.clone()),
        old_mode: "000000".to_owned(),
        new_mode: format_mode(blob.mode),
        old_oid: zero_oid(),
        new_oid: blob.oid,
        score: None,
    }
}

/// Populate a work tree and index from `tree_oid` (empty index or wholesale replace).
fn checkout_populated_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CheckoutOutcome> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    let config = repo.config()?;
    let protect = ProtectOptions::from_config(config.as_ref());
    let ignorecase = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let precompose = repo.pathspec_precompose_enabled();

    let total = diff_tree_path_change_count(repo.odb(), None, Some(tree_oid), "")?;
    progress.start(
        ProgressPhase::CheckingOutFiles,
        Some(u64::try_from(total).unwrap_or(u64::MAX)),
    );

    let mut loaded_index = repo.load_index()?;
    let index_mut = &mut loaded_index;

    let rules = Rc::new(
        if tree_contains_dot_gitattributes(repo.odb(), tree_oid, "", protect)? {
            let mut prospective = Index::new();
            for entry in crate::porcelain::merge::tree_to_index_entries(repo, tree_oid, "")? {
                prospective.add_or_replace(entry);
            }
            prospective.sort();
            WorktreeRules::from_checkout_prospective_index(repo, &prospective, config.clone())?
        } else {
            checkout_worktree_rules(repo, index_mut, &[], config.clone())?
        },
    );
    let conv = rules.conversion().clone();

    let mut walk = PopulatedTreeWalk::new(repo.odb(), tree_oid, protect)?;
    let mut chunk = Vec::with_capacity(POPULATED_TREE_CHECKOUT_CHUNK);
    // Chunks only update the in-memory index. Building the cache tree walks the whole tree and
    // writing the index serializes every entry, so both happen once, after the last chunk.
    let mut apply = CheckoutApplyOptions {
        parallel_prepare_blobs: false,
        checkout_result_tree: None,
        defer_index_disk_write: true,
        precomputed_worktree_rules: Some(Rc::clone(&rules)),
        sparse_paths_applied: true,
        ..CheckoutApplyOptions::default()
    };
    let sparse = SparseCheckout::load(repo)?;
    let mut bulk_index_rewrite = false;

    while walk.fill_chunk(&mut chunk)? {
        crate::progress::poll_cancel(progress, cancel)?;
        let chunk_changes: Vec<DiffEntry> =
            chunk.iter().map(populated_blob_to_added_diff).collect();
        let changes = apply_sparse_tree_changes_to_index(
            sparse.as_ref(),
            work_tree,
            index_mut,
            &chunk_changes,
            &mut apply,
        );
        prefetch_promisor_blobs_for_checkout(repo, &changes)?;
        let mut chunk_progress = ChunkProgressSink(progress);
        let bulk_write = bulk_write_checkout_eligible(&changes, rules.as_ref())
            && tree_changes_are_all_additions(&changes);
        bulk_index_rewrite = chunk.len() == POPULATED_TREE_CHECKOUT_CHUNK
            || bulk_write
            || changes.len() >= BULK_INDEX_REWRITE_MIN;
        if bulk_write {
            checkout_bulk_added_tree_changes(
                repo,
                work_tree,
                &changes,
                index_mut,
                rules.as_ref(),
                &config,
                &conv,
                apply.clone(),
                precompose,
                ignorecase,
                &mut chunk_progress,
                cancel,
            )?;
        } else {
            apply.skip_bulk_checkout_fast_path = true;
            checkout_tree_changes_with_progress(
                repo,
                &changes,
                &mut chunk_progress,
                cancel,
                Some(index_mut),
                apply.clone(),
                None,
            )?;
            apply.skip_bulk_checkout_fast_path = false;
        }
        apply.skip_parallel_write_ramp = true;
    }

    attach_checkout_result_cache_tree(repo, index_mut, tree_oid)?;
    write_checkout_index(repo, index_mut, bulk_index_rewrite, None)?;

    progress.finish();
    crate::progress::poll_cancel(progress, cancel)?;

    let path_collisions = if ignorecase {
        crate::path_icase::icase_checkout_collision_paths(index_mut, true)
    } else {
        Vec::new()
    };
    Ok(CheckoutOutcome { path_collisions })
}

fn checkout_between_trees_slow(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: Option<&ObjectId>,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    let changes = diff_trees(repo.odb(), from, to, "")?;
    let mut apply = CheckoutApplyOptions::default();
    if let Some(tree) = to {
        apply.checkout_result_tree = Some(*tree);
        if from.is_none() {
            apply.parallel_prepare_blobs = true;
        }
    }
    checkout_tree_changes_with_progress(repo, &changes, progress, cancel, None, apply, None)
}

/// Apply the change from tree `from` to tree `to` onto the working tree and
/// index, then write the index.
///
/// This is the reusable core of a clean branch switch / fast-forward: it writes,
/// updates, and deletes only the paths that differ between the two trees, and
/// rebuilds the matching index entries (with fresh stat data so a subsequent
/// status reports a clean tree). Unchanged paths are left untouched.
///
/// The caller is responsible for policy: this function assumes the tracked
/// working tree is **clean** relative to `from` (no staged or unstaged changes
/// that would be silently overwritten) and that no untracked file sits where an
/// added path needs to land. It performs the mechanical apply only.
pub fn checkout_between_trees(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: &ObjectId,
) -> Result<()> {
    checkout_between_trees_with_progress(repo, from, to, &mut NullProgress, &NeverCancel)?;
    Ok(())
}

/// Like [`checkout_between_trees`], reporting checkout progress to `progress`.
///
/// Populated-tree checkouts emit [`ProgressPhase::CheckingOutFiles`] with the
/// exact blob count from the destination tree; tree-to-tree diffs report the
/// number of path changes.
pub fn checkout_between_trees_with_progress(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: &ObjectId,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<CheckoutOutcome> {
    if from.is_none() {
        return checkout_populated_tree(repo, to, progress, cancel);
    }
    checkout_between_trees_slow(repo, from, Some(to), progress, cancel)?;
    Ok(CheckoutOutcome::default())
}

/// Replace the index and tracked worktree so they match `target_tree` exactly.
///
/// Used by hard reset. When `head_tree` differs from `target_tree`, only paths that
/// changed between the two trees are written first (same as branch checkout); paths
/// that stayed the same are left on disk when already up to date. When both trees
/// are equal (reset to the current commit), only dirty or mismatched paths are
/// rewritten. Missing promised blobs needed for writes and verification (including
/// attribute dependencies) are fetched together before content reads. Sparse-excluded
/// paths and gitlinks do not request blob content; lazy-fetch opt-outs still apply.
/// Tracked paths absent from `target_tree` are removed and the index is
/// rebuilt without unmerged stages.
///
/// # Errors
///
/// Returns an error when the repository has no work tree, required promised objects
/// cannot be fetched, or checkout I/O fails.
pub fn checkout_hard_to_tree_from_head_tree(
    repo: &Repository,
    head_tree: &ObjectId,
    target_tree: &ObjectId,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    use crate::porcelain::index_from_tree::index_from_tree_reusing_cache_tree;

    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    let mut pre_reset_index =
        crate::porcelain::index_from_tree::load_index_for_tree_transition(repo, target_tree)?;
    let trees_differ = head_tree != target_tree;
    let changes = if trees_differ {
        diff_trees(repo.odb(), Some(head_tree), Some(target_tree), "")?
    } else {
        Vec::new()
    };
    crate::porcelain::index_from_tree::expand_sparse_for_tree_changes(
        repo,
        &mut pre_reset_index,
        &changes,
    )?;
    let mut target = index_from_tree_reusing_cache_tree(repo, target_tree, &pre_reset_index)?;
    let target_cache_tree = target.cache_tree.take();
    let target_entries = std::mem::take(&mut target.entries);
    // Raciness is judged against the index as it was before the reset: the tree-diff checkout
    // below rewrites the index, which would make racily clean entries look settled.
    let pre_reset_index_mtime = crate::diff::index_mtime_for_diff(&pre_reset_index, None);
    let deleted_abs_paths = remove_index_paths_outside_tree_diff(
        &pre_reset_index,
        work_tree,
        &target_entries,
        &changes,
    );

    let mut written_by_tree_diff: BTreeSet<Vec<u8>> = BTreeSet::new();
    for change in &changes {
        if change.status == DiffStatus::Deleted {
            continue;
        }
        if let Some(path) = &change.new_path {
            written_by_tree_diff.insert(crate::repo_relpath::storage_bytes(path).into_owned());
        }
    }
    let config = repo.config()?;
    let sparse = SparseCheckout::load(repo)?;
    let stat_positions = hard_reset_stat_probe_positions(
        &pre_reset_index,
        &target_entries,
        trees_differ,
        &written_by_tree_diff,
        sparse.as_ref(),
    );
    let worktree_stats = scan_index_blob_stats_at(
        repo,
        work_tree,
        &pre_reset_index,
        config.as_ref(),
        &stat_positions,
    )?;
    let stat_policy = crate::index::StatMatchPolicy::from_config(Some(config.as_ref()));
    let trust_filemode = config
        .get_bool("core.filemode")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let mut actions: Vec<HardResetEntryAction> = target_entries
        .iter()
        .map(|te| {
            if sparse
                .as_ref()
                .is_some_and(|sparse| !sparse.includes_index_path(&te.path))
            {
                return HardResetEntryAction::SkipWorktree;
            }
            if te.mode == MODE_GITLINK {
                return HardResetEntryAction::Gitlink;
            }
            if trees_differ && written_by_tree_diff.contains(&te.path) {
                return HardResetEntryAction::Written(None);
            }
            let pos = pre_reset_index.find_entry_pos(&te.path, 0);
            let existing = pos.map(|pos| (pos, &pre_reset_index.entries[pos]));
            let matches_target = existing.filter(|(_, ie)| ie.oid == te.oid && ie.mode == te.mode);
            let Some((pos, existing)) = matches_target else {
                return HardResetEntryAction::Write;
            };
            match worktree_stats[pos] {
                Some(stat)
                    if hard_reset_entry_stat_clean(
                        existing,
                        &stat,
                        stat_policy,
                        trust_filemode,
                        pre_reset_index_mtime,
                    ) =>
                {
                    HardResetEntryAction::Keep(pos)
                }
                _ => HardResetEntryAction::Verify,
            }
        })
        .collect();
    // Fetch repair blobs together with tree-diff writes before attribute resolution can
    // trigger isolated lazy reads. Planning uses only tree/index metadata and disk stats.
    let needs_content = |action: &HardResetEntryAction| {
        matches!(
            action,
            HardResetEntryAction::Write
                | HardResetEntryAction::Verify
                | HardResetEntryAction::Written(_)
        )
    };
    let blobs = target_entries
        .iter()
        .zip(&actions)
        .filter(|(_, action)| needs_content(action))
        .map(|(entry, _)| entry.oid);
    let attributes = pre_reset_index
        .entries
        .iter()
        .chain(&target_entries)
        .filter(|entry| {
            entry.stage() == 0
                && (entry.path == b".gitattributes" || entry.path.ends_with(b"/.gitattributes"))
                && entry.mode != MODE_GITLINK
                && target_entries.iter().zip(&actions).any(|(target, action)| {
                    needs_content(action)
                        && target
                            .path
                            .starts_with(&entry.path[..entry.path.len() - b".gitattributes".len()])
                })
        })
        .map(|entry| entry.oid);
    prefetch_promisor_blobs(repo, blobs.chain(attributes))?;
    // Hard reset reapplies the sparse definition. Tracked files that caused skip-worktree
    // to be cleared are removed again; paths still marked skip-worktree (including the
    // expectFilesOutsideOfPatterns opt-out) and untracked files are left alone.
    let sparse_removals: Vec<_> = pre_reset_index
        .entries
        .iter()
        .filter(|entry| {
            !entry.skip_worktree()
                && !entry.is_sparse_directory_placeholder()
                && sparse
                    .as_ref()
                    .is_some_and(|s| !s.includes_index_path(&entry.path))
        })
        .map(|entry| (entry.path.clone(), entry.mode))
        .collect();
    let hard_reset_apply = if changes.is_empty() {
        None
    } else {
        Some(CheckoutApplyOptions {
            parallel_prepare_blobs: true,
            defer_index_disk_write: true,
            ..CheckoutApplyOptions::default()
        })
    };
    let mut index = pre_reset_index;

    if let Some(apply) = hard_reset_apply {
        checkout_tree_changes_with_progress(
            repo,
            &changes,
            progress,
            cancel,
            Some(&mut index),
            apply,
            None,
        )?;
    }

    for (path, mode) in sparse_removals {
        let abs = crate::index::worktree_path_from_index_rel(work_tree, &path);
        if mode == MODE_GITLINK {
            remove_uninitialized_gitlink_worktree(&abs);
        } else {
            match fs::remove_file(&abs) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::Io(err)),
            }
        }
        remove_empty_parent_dirs_batch(work_tree, &[abs]);
    }
    index.source_mtime = pre_reset_index_mtime;
    let mut delayed = DelayedProcessCheckout::default();
    let mut pending_delayed: Vec<DelayedCheckoutTarget> = Vec::new();
    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(target_entries.len());
    let protect = ProtectOptions::from_config(config.as_ref());
    let mut dir_cache = LeadingDirCache::with_protect(protect);

    if trees_differ {
        for (entry, action) in target_entries.iter().zip(&mut actions) {
            match action {
                HardResetEntryAction::Keep(_) | HardResetEntryAction::Written(_) => {
                    let pos = index.find_entry_pos(&entry.path, 0).filter(|&pos| {
                        let existing = &index.entries[pos];
                        existing.oid == entry.oid
                            && existing.mode == entry.mode
                            && !existing.stat_cache_unset()
                    });
                    *action = if matches!(action, HardResetEntryAction::Written(_)) {
                        HardResetEntryAction::Written(pos)
                    } else if let Some(pos) = pos {
                        HardResetEntryAction::Keep(pos)
                    } else {
                        HardResetEntryAction::Write
                    };
                }
                _ => {}
            }
        }
    }
    let mut rules = actions
        .iter()
        .any(|action| matches!(action, HardResetEntryAction::Verify))
        .then(|| WorktreeRules::from_parts(repo, &index, config.clone()))
        .transpose()?;
    if let Some(rules) = &rules {
        verify_hard_reset_candidates(
            repo,
            work_tree,
            &index,
            &target_entries,
            rules,
            &mut actions,
        )?;
    }

    progress.start(
        ProgressPhase::CheckingOutFiles,
        Some(u64::try_from(target_entries.len()).unwrap_or(u64::MAX)),
    );
    for (mut te, action) in target_entries.into_iter().zip(actions) {
        crate::progress::poll_cancel(progress, cancel)?;
        let abs = crate::index::worktree_path_from_index_rel(work_tree, &te.path);
        let mode = te.mode;
        match action {
            HardResetEntryAction::SkipWorktree => {
                te.set_skip_worktree(true);
                new_entries.push(te);
                continue;
            }
            HardResetEntryAction::Keep(pos) | HardResetEntryAction::Written(Some(pos)) => {
                new_entries.push(index_entry_with_recorded_stat(&index.entries[pos]));
                continue;
            }
            HardResetEntryAction::Written(None) | HardResetEntryAction::VerifiedUpToDate => {
                let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                new_entries.push(entry_from_metadata(&meta, te.path.as_slice(), te.oid, mode));
                continue;
            }
            HardResetEntryAction::Gitlink
            | HardResetEntryAction::Write
            | HardResetEntryAction::Verify => {}
        }
        if mode == MODE_GITLINK {
            if trees_differ && written_by_tree_diff.contains(&te.path) {
                let abs = abs.clone();
                let mut entry = entry_from_stat(&abs, te.path.as_slice(), te.oid, MODE_GITLINK)?;
                entry.size = 0;
                new_entries.push(entry);
                continue;
            }
            if let Some(existing) = index.get(te.path.as_slice(), 0) {
                if existing.oid == te.oid
                    && existing.mode == MODE_GITLINK
                    && gitlink_worktree_matches_index(&abs)
                {
                    let abs = abs.clone();
                    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                    let mut entry =
                        entry_from_metadata(&meta, te.path.as_slice(), te.oid, MODE_GITLINK);
                    entry.size = 0;
                    new_entries.push(entry);
                    continue;
                }
            }
            let path = crate::repo_relpath::storage_string(&te.path);
            materialize_gitlink_worktree(work_tree, &path, &mut dir_cache)?;
            let abs = abs.clone();
            let mut entry = entry_from_stat(&abs, te.path.as_slice(), te.oid, MODE_GITLINK)?;
            entry.size = 0;
            new_entries.push(entry);
            continue;
        }

        let rules = match &mut rules {
            Some(rules) => rules,
            empty => empty.insert(WorktreeRules::from_parts(repo, &index, config.clone())?),
        };
        let path = crate::repo_relpath::storage_string(&te.path);
        let change = diff_entry_for_worktree_blob_refresh(work_tree, &path, te.oid, mode);
        let Some(object) = read_blob_unless_streamed(
            repo,
            rules.conversion(),
            &rules.file_attrs(&path, false),
            mode,
            &te.oid,
            true,
        )?
        else {
            new_entries.push(stream_checkout_entry(
                repo,
                work_tree,
                &path,
                mode,
                &change,
                &mut dir_cache,
                te.oid,
            )?);
            continue;
        };
        let worktree_bytes = worktree_bytes_from_index_blob(
            repo,
            rules,
            &path,
            mode,
            &object.data,
            &te.oid,
            None,
            Some(&mut delayed),
        )?;
        let Some(worktree_bytes) = worktree_bytes else {
            pending_delayed.push(DelayedCheckoutTarget {
                path: path.clone(),
                mode,
                oid: te.oid,
                change: change.clone(),
            });
            continue;
        };
        let entry = write_checkout_entry(
            work_tree,
            &path,
            WorktreeFileContent::Bytes(&worktree_bytes),
            mode,
            &change,
            &mut dir_cache,
            te.oid,
        )?;
        new_entries.push(entry);
        progress.inc(1);
    }

    if let Some(rules) = &rules {
        finish_delayed_checkouts(
            repo,
            rules,
            work_tree,
            &mut delayed,
            &pending_delayed,
            &mut dir_cache,
            &mut new_entries,
        )?;
    }

    remove_empty_parent_dirs_batch(work_tree, &deleted_abs_paths);
    progress.finish();

    let mtime_hint = Some(crate::index::stage0_mtime_max(&new_entries));
    let mut new_index = Index::new();
    new_index.entries = new_entries;
    new_index.sort();
    new_index.sparse_directories = new_index.has_sparse_directory_placeholders();
    if let Some(cache_tree) = target_cache_tree
        .filter(|ct| usize::try_from(ct.entry_count).ok() == Some(new_index.entries.len()))
    {
        new_index.set_cache_tree(cache_tree);
    }
    if trees_differ && !written_by_tree_diff.is_empty() {
        write_checkout_index(repo, &mut new_index, false, mtime_hint)?;
    } else {
        repo.write_index(&mut new_index)?;
    }
    Ok(())
}

/// Index positions that need a worktree stat before hard reset can choose keep vs verify.
fn hard_reset_stat_probe_positions(
    index: &Index,
    target_entries: &[IndexEntry],
    trees_differ: bool,
    written_by_tree_diff: &BTreeSet<Vec<u8>>,
    sparse: Option<&SparseCheckout>,
) -> Vec<usize> {
    let mut positions = Vec::new();
    for te in target_entries {
        if sparse.is_some_and(|sparse| {
            sparse.checkout_skips(index, &crate::repo_relpath::storage_string(&te.path))
        }) {
            continue;
        }
        if te.mode == MODE_GITLINK {
            continue;
        }
        if trees_differ && written_by_tree_diff.contains(&te.path) {
            continue;
        }
        let Some(pos) = index.find_entry_pos(&te.path, 0) else {
            continue;
        };
        let existing = &index.entries[pos];
        if existing.oid == te.oid && existing.mode == te.mode {
            positions.push(pos);
        }
    }
    positions
}

/// What a hard reset does with one target-tree entry.
enum HardResetEntryAction {
    /// Outside the sparse checkout: record the index row with `skip-worktree`, no work-tree I/O.
    SkipWorktree,
    /// Gitlink: handled by the submodule placeholder logic.
    Gitlink,
    /// Written by the tree-diff checkout; reuse the index row it recorded (position in the
    /// reloaded index) when that row has stat data, else stat the path.
    Written(Option<usize>),
    /// Unchanged and stat-clean: keep the worktree file and its index row (position in the
    /// reloaded index).
    Keep(usize),
    /// Unchanged but stat-dirty, racy or unscanned: compare content before deciding.
    Verify,
    /// Content check passed: keep the worktree file.
    VerifiedUpToDate,
    /// Write the blob to the worktree.
    Write,
}

/// Stage-0 copy of an index row whose stat data was recorded by a path stat (Git's own
/// write, or this checkout), so the rebuilt index never stores directory-scan stat fields.
fn index_entry_with_recorded_stat(entry: &IndexEntry) -> IndexEntry {
    let mut entry = entry.clone();
    entry.flags = entry.path.len().min(0xFFF) as u16;
    entry.flags_extended = None;
    entry.base_index_pos = 0;
    entry
}

/// Whether a directory-scan stat proves the worktree file still matches `existing` without
/// reading it: a regular file (a directory can share size and mtime with the file it
/// replaced), same stat data and (with `core.filemode`) executable bit, and the entry is not
/// racily clean.
fn hard_reset_entry_stat_clean(
    existing: &IndexEntry,
    stat: &crate::worktree_scan::BlobDiskStat,
    stat_policy: crate::index::StatMatchPolicy,
    trust_filemode: bool,
    index_mtime: Option<(u32, u32)>,
) -> bool {
    is_regular_git_mode(existing.mode)
        && !stat.is_dir
        && is_regular_git_mode(stat.git_mode)
        && (!trust_filemode || stat.git_mode == existing.mode)
        && crate::diff::stat_matches_blob_stat(existing, stat, stat_policy)
        && !crate::diff::entry_is_racy_with_policy(existing, index_mtime, stat_policy)
}

/// Stat selected stage-0 blobs in `index` with one directory-grouped pass.
///
/// Returns one slot per index entry: `Some` when the path exists on disk, `None` when it is
/// missing, unreadable, or not part of the scan (gitlinks, skip-worktree, unmerged).
fn scan_index_blob_stats_at(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    config: &ConfigSet,
    positions: &[usize],
) -> Result<Vec<Option<crate::worktree_scan::BlobDiskStat>>> {
    use crate::worktree_scan::{
        collect_blob_lookup_batches, group_blob_entries_by_dir, BlobDiskLookup, BlobScanSparseMode,
        WorktreeBlobScanOptions,
    };

    let mut stats = vec![None; index.entries.len()];
    if positions.is_empty() {
        return Ok(stats);
    }
    let original_by_path: std::collections::HashMap<Vec<u8>, usize> = positions
        .iter()
        .filter_map(|&pos| index.entries.get(pos).map(|e| (e.path.clone(), pos)))
        .collect();
    let mut subset = Index::new();
    subset.entries_sorted = index.entries_sorted;
    for &pos in positions {
        if let Some(entry) = index.entries.get(pos) {
            subset.entries.push(entry.clone());
        }
    }
    subset.sort();
    let plan = group_blob_entries_by_dir(&subset, false);
    if plan.is_empty() {
        return Ok(stats);
    }
    let preload_index = config
        .get_bool("core.preloadIndex")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let dir_root = work_tree.to_path_buf();
    let file_root = work_tree.to_path_buf();
    let batches = collect_blob_lookup_batches(
        &subset,
        &plan,
        WorktreeBlobScanOptions {
            preload_index,
            stat_parallel_threads: None,
        },
        &repo.thread_policy(),
        config,
        move |dir: &[u8]| crate::index::worktree_path_from_index_rel(&dir_root, dir),
        move |rel: &[u8]| crate::index::worktree_path_from_index_rel(&file_root, rel),
        |_| false,
        BlobScanSparseMode::Dense,
    )?;
    for (entry_index, lookup) in batches.into_iter().flatten() {
        let Some(entry) = subset.entries.get(entry_index) else {
            continue;
        };
        let Some(&orig) = original_by_path.get(&entry.path) else {
            continue;
        };
        if let BlobDiskLookup::Present(stat) = lookup {
            stats[orig] = Some(stat);
        }
    }
    Ok(stats)
}

/// Resolve [`HardResetEntryAction::Verify`] entries by comparing worktree content with the
/// index blob, in parallel (racy files are typically a whole batch just written by a
/// previous checkout, and each check opens and reads the file).
fn verify_hard_reset_candidates(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    target_entries: &[IndexEntry],
    rules: &WorktreeRules,
    actions: &mut [HardResetEntryAction],
) -> Result<()> {
    let candidates: Vec<usize> = actions
        .iter()
        .enumerate()
        .filter(|(_, action)| matches!(action, HardResetEntryAction::Verify))
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let config = repo.config()?;
    let mut options = repo
        .thread_policy()
        .resolve(ParallelOp::IndexRefresh, config.as_ref());
    options.min_total_bytes = None;
    let filter_process = rules.filter_process();
    let odb = repo.odb();
    let git_dir = repo.git_dir();
    let up_to_date = try_par_map(&candidates, options, 0, |&i| {
        let te = &target_entries[i];
        let Some(existing) = index.get(&te.path, 0) else {
            return Ok(false);
        };
        crate::diff::path_checkout_skip_blob_write_when_up_to_date_with_config(
            odb,
            git_dir,
            work_tree,
            index,
            existing,
            &te.oid,
            te.mode,
            &crate::repo_relpath::storage_string(&te.path),
            Some(filter_process),
            Some(config.as_ref()),
        )
    })
    .map_err(|ParallelHashError::Task(e)| e)?;
    for (i, keep) in candidates.into_iter().zip(up_to_date) {
        actions[i] = if keep {
            HardResetEntryAction::VerifiedUpToDate
        } else {
            HardResetEntryAction::Write
        };
    }
    Ok(())
}

/// Remove indexed paths that leave the index but are not tree-diff deletions.
///
/// Tree-diff deletions of plain blobs are left to the tree-diff checkout, which removes whole
/// directories at once and cleans up empty parents in one batch. Paths that only exist in the
/// index (staged additions), gitlinks and unmerged paths are removed here. Returns the
/// removed paths for empty-parent cleanup.
fn remove_index_paths_outside_tree_diff(
    index: &Index,
    work_tree: &Path,
    target_entries: &[IndexEntry],
    changes: &[DiffEntry],
) -> Vec<PathBuf> {
    let target_paths: HashSet<&[u8]> = target_entries.iter().map(|e| e.path.as_slice()).collect();
    let tree_deleted: HashSet<Vec<u8>> = changes
        .iter()
        .filter(|change| change.status == DiffStatus::Deleted)
        .filter_map(|change| change.old_path.as_deref())
        .map(checkout_stored_index_path_bytes)
        .collect();
    let mut removed = Vec::new();
    let mut previous: Option<&[u8]> = None;
    for entry in &index.entries {
        let path = entry.path.as_slice();
        if previous == Some(path) || target_paths.contains(path) {
            continue;
        }
        previous = Some(path);
        let stage0 = index.get(path, 0);
        if tree_deleted.contains(path) && stage0.is_some_and(|e| e.mode != MODE_GITLINK) {
            continue;
        }
        let abs = crate::index::worktree_path_from_index_rel(work_tree, path);
        if stage0.is_some_and(|e| e.mode == MODE_GITLINK) {
            remove_uninitialized_gitlink_worktree(&abs);
        } else {
            let _ = fs::remove_file(&abs);
        }
        removed.push(abs);
    }
    removed
}

/// Apply a precomputed tree-to-tree diff to the working tree and index.
///
/// Callers such as `grit switch` can reuse one [`diff_trees`] result for both
/// untracked guards and the mechanical apply.
pub fn checkout_tree_changes(repo: &Repository, changes: &[DiffEntry]) -> Result<()> {
    checkout_tree_changes_with_progress(
        repo,
        changes,
        &mut NullProgress,
        &NeverCancel,
        None,
        CheckoutApplyOptions::default(),
        None,
    )
}

/// Options for [`checkout_tree_changes_with_index_opts`].
#[derive(Clone, Default)]
pub(crate) struct CheckoutApplyOptions {
    /// Read blob ODB objects in parallel when preparing write jobs.
    ///
    /// Only set when every OID is known to exist (e.g. tree-to-tree switch); general checkout
    /// must keep serial prepare so missing blobs flush earlier writes before failing.
    pub parallel_prepare_blobs: bool,
    /// Override `checkout.thresholdForParallelism` for this apply (`None` = repository config).
    pub checkout_parallel_threshold: Option<i64>,
    /// When true, wholesale directory removal skips the on-disk safety walk (switch already
    /// verified no untracked paths under those directories).
    pub trust_wholesale_dir_removal: bool,
    /// Wholesale delete directories that passed the on-disk safety walk during switch prepare.
    pub preverified_wholesale_dirs: BTreeSet<String>,
    /// When true, [`preverified_wholesale_dirs`] is authoritative and checkout skips safety walks.
    pub wholesale_safety_precomputed: bool,
    /// Skip bulk add/delete fast paths (used when bulk checkout hits a delayed filter).
    pub skip_bulk_checkout_fast_path: bool,
    /// When set, the index should match this tree after checkout (updates cache-tree root).
    pub checkout_result_tree: Option<crate::objects::ObjectId>,
    /// Apply checkout to the worktree but defer index disk writes until the caller finishes.
    pub defer_index_disk_write: bool,
    /// Pre-built attribute/conversion context (populated-tree checkout with `.gitattributes`).
    pub precomputed_worktree_rules: Option<Rc<WorktreeRules>>,
    /// The caller already split off sparse-checkout skipped paths; every change left is
    /// applied to the work tree.
    pub sparse_paths_applied: bool,
    /// Index rows (with `skip-worktree` set) for sparse-checkout skipped paths, merged into the
    /// index together with the rows checkout writes.
    pub skip_worktree_rows: Vec<IndexEntry>,
    /// Flush parallel writes in full-size chunks from the start instead of ramping up from a
    /// few files. Set by callers that already observed cancellation between earlier calls, so
    /// only their first call needs the small chunks that stop quickly after a cancel.
    pub skip_parallel_write_ramp: bool,
}

/// Like [`checkout_tree_changes`], but updates `index` in place instead of loading from disk.
///
/// Callers such as `grit switch` use this after a tree-diff guard pass that already loaded the index.
pub fn checkout_tree_changes_with_index(
    repo: &Repository,
    changes: &[DiffEntry],
    index: &mut Index,
) -> Result<()> {
    checkout_tree_changes_with_index_opts(
        repo,
        changes,
        index,
        &mut NullProgress,
        &NeverCancel,
        CheckoutApplyOptions::default(),
        None,
    )
}

/// Like [`checkout_tree_changes_with_index`], with apply-time tuning for trusted callers.
pub(crate) fn checkout_tree_changes_with_index_opts(
    repo: &Repository,
    changes: &[DiffEntry],
    index: &mut Index,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    apply: CheckoutApplyOptions,
    attribute_changes: Option<&[DiffEntry]>,
) -> Result<()> {
    checkout_tree_changes_with_progress(
        repo,
        changes,
        progress,
        cancel,
        Some(index),
        apply,
        attribute_changes,
    )
}

/// Top-level directories whose tracked files are entirely removed by `delete_paths`.
pub(crate) fn wholesale_deleted_directories(
    index: &Index,
    delete_paths: &[String],
) -> BTreeSet<String> {
    use std::collections::BTreeMap;
    if delete_paths.len() < 8 {
        return BTreeSet::new();
    }
    let mut deleted_under: BTreeMap<String, usize> = BTreeMap::new();
    for path in delete_paths {
        let Some((dir, _rest)) = path.split_once('/') else {
            continue;
        };
        *deleted_under.entry(dir.to_owned()).or_default() += 1;
    }
    if deleted_under.is_empty() {
        return BTreeSet::new();
    }
    let delete_set: HashSet<String> = delete_paths.iter().cloned().collect();
    let mut wholesale = BTreeSet::new();
    if index.entries_sorted {
        for (dir, del_count) in &deleted_under {
            if *del_count == 0 {
                continue;
            }
            let prefix = format!("{dir}/");
            let prefix_bytes = prefix.as_bytes();
            let start = index
                .entries
                .partition_point(|e| e.path.as_slice() < prefix_bytes);
            let mut indexed_count = 0usize;
            let mut all_scheduled = true;
            for entry in index.entries[start..].iter() {
                if !entry.path.starts_with(prefix_bytes) {
                    break;
                }
                if entry.stage() != 0 {
                    continue;
                }
                indexed_count += 1;
                let Ok(path) = std::str::from_utf8(&entry.path) else {
                    all_scheduled = false;
                    break;
                };
                if !delete_path_is_scheduled(path, &delete_set, false) {
                    all_scheduled = false;
                    break;
                }
            }
            if all_scheduled && indexed_count == *del_count {
                wholesale.insert(dir.clone());
            }
        }
        return wholesale;
    }
    let mut indexed_under: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked: BTreeSet<String> = BTreeSet::new();
    for entry in &index.entries {
        if entry.stage() != 0 {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&entry.path) else {
            continue;
        };
        let Some((dir, _rest)) = path.split_once('/') else {
            continue;
        };
        if !deleted_under.contains_key(dir) {
            continue;
        }
        *indexed_under.entry(dir.to_owned()).or_default() += 1;
        if !delete_path_is_scheduled(path, &delete_set, false) {
            blocked.insert(dir.to_owned());
        }
    }
    for (dir, del_count) in deleted_under {
        if del_count == 0 || blocked.contains(&dir) {
            continue;
        }
        if indexed_under.get(&dir).copied() == Some(del_count) {
            wholesale.insert(dir);
        }
    }
    wholesale
}

/// True when every file under `work_tree`/`dir` is listed in `delete_paths` (Git keeps untracked/ignored extras).
fn delete_path_is_scheduled(rel: &str, delete_paths: &HashSet<String>, ignorecase: bool) -> bool {
    if ignorecase {
        delete_paths
            .iter()
            .any(|del| paths_equal(rel.as_bytes(), del.as_bytes(), true))
    } else {
        delete_paths.contains(rel)
    }
}

/// Untracked and ignored paths collected under wholesale delete trees before switch checkout.
pub(crate) struct WholesaleWorktreeScan<'a> {
    pub untracked: &'a [String],
    pub ignored: &'a [String],
    pub ignorecase: bool,
}

fn wholesale_dir_has_extra_paths(dir: &str, scan: &WholesaleWorktreeScan<'_>) -> bool {
    scan.untracked
        .iter()
        .chain(scan.ignored.iter())
        .any(|path| path_is_under_dir(path, dir, scan.ignorecase))
}

fn path_is_under_dir(path: &str, dir: &str, ignorecase: bool) -> bool {
    let prefix = format!("{dir}/");
    if ignorecase {
        path.eq_ignore_ascii_case(dir)
            || path
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(&prefix))
    } else {
        path == dir || path.starts_with(&prefix)
    }
}

/// Wholesale directories that passed switch prepare safety checks.
#[derive(Debug, Default)]
pub(crate) struct WholesalePrecomputeResult {
    pub safe_directories: BTreeSet<String>,
}

pub(crate) fn wholesale_directory_safe_to_remove(
    work_tree: &Path,
    dir: &str,
    delete_paths: &HashSet<String>,
    ignorecase: bool,
) -> bool {
    let abs_dir = work_tree.join(dir);
    let mut stack = vec![abs_dir];
    while let Some(current) = stack.pop() {
        let read_dir = match fs::read_dir(&current) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                let _ = e;
                return false;
            }
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => return false,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() && !file_type.is_symlink() {
                return false;
            }
            let Ok(rel) = path.strip_prefix(work_tree) else {
                return false;
            };
            let rel = rel.to_string_lossy();
            if !delete_path_is_scheduled(rel.as_ref(), delete_paths, ignorecase) {
                return false;
            }
        }
    }
    true
}

/// On-disk safety walk for wholesale directory removal, parallelized for switch prepare.
pub(crate) fn precompute_wholesale_safe_directories(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    delete_paths: &[String],
    ignorecase: bool,
    worktree_scan: Option<&WholesaleWorktreeScan<'_>>,
    gitlink_deletes: &HashSet<String>,
) -> WholesalePrecomputeResult {
    let candidates = wholesale_deleted_directories(index, delete_paths);
    if candidates.is_empty() {
        return WholesalePrecomputeResult::default();
    }
    if let Some(scan) = worktree_scan {
        let filtered: BTreeSet<String> = candidates
            .into_iter()
            .filter(|dir| !wholesale_dir_has_extra_paths(dir, scan))
            .collect();
        return WholesalePrecomputeResult {
            safe_directories: filter_wholesale_dirs_with_populated_gitlinks(
                work_tree,
                filtered,
                gitlink_deletes,
            ),
        };
    }
    let delete_set: HashSet<String> = delete_paths.iter().cloned().collect();
    let needs_walk: Vec<String> = candidates.into_iter().collect();
    let default_cfg = ConfigSet::new();
    let config = repo.config().ok();
    let cfg = config.as_deref().unwrap_or(&default_cfg);
    let options = repo.thread_policy().resolve(ParallelOp::Checkout, cfg);
    let safe: BTreeSet<String> = if needs_walk.len() >= 2 && options.threads.get() > 1 {
        let wt = work_tree.to_path_buf();
        let delete_set = delete_set.clone();
        let safe_flags = try_par_map(&needs_walk, options, 0, |dir| {
            Ok(wholesale_directory_safe_to_remove(
                &wt,
                dir,
                &delete_set,
                ignorecase,
            ))
        })
        .unwrap_or_else(|_: ParallelHashError<()>| {
            needs_walk
                .iter()
                .map(|dir| {
                    wholesale_directory_safe_to_remove(work_tree, dir, &delete_set, ignorecase)
                })
                .collect()
        });
        needs_walk
            .into_iter()
            .zip(safe_flags)
            .filter_map(|(dir, safe)| safe.then_some(dir))
            .collect()
    } else {
        needs_walk
            .into_iter()
            .filter(|dir| {
                wholesale_directory_safe_to_remove(work_tree, dir, &delete_set, ignorecase)
            })
            .collect()
    };
    WholesalePrecomputeResult {
        safe_directories: filter_wholesale_dirs_with_populated_gitlinks(
            work_tree,
            safe,
            gitlink_deletes,
        ),
    }
}

fn wholesale_deleted_directories_on_disk(
    repo: &Repository,
    work_tree: &Path,
    index: &Index,
    delete_paths: &[String],
    ignorecase: bool,
    apply: &CheckoutApplyOptions,
    gitlink_deletes: &HashSet<String>,
) -> BTreeSet<String> {
    let candidates = wholesale_deleted_directories(index, delete_paths);
    if apply.trust_wholesale_dir_removal {
        return filter_wholesale_dirs_with_populated_gitlinks(
            work_tree,
            candidates,
            gitlink_deletes,
        );
    }
    if apply.wholesale_safety_precomputed {
        let filtered = candidates
            .into_iter()
            .filter(|dir| apply.preverified_wholesale_dirs.contains(dir))
            .collect();
        return filter_wholesale_dirs_with_populated_gitlinks(work_tree, filtered, gitlink_deletes);
    }
    let delete_set: HashSet<String> = delete_paths.iter().cloned().collect();
    let needs_walk: Vec<String> = candidates.into_iter().collect();
    let mut trusted = BTreeSet::new();
    if needs_walk.is_empty() {
        return trusted;
    }
    let config = repo.config().ok();
    let default_cfg = ConfigSet::new();
    let cfg = config.as_deref().unwrap_or(&default_cfg);
    let options = repo.thread_policy().resolve(ParallelOp::Checkout, cfg);
    if needs_walk.len() >= 4 && options.threads.get() > 1 {
        let wt = work_tree.to_path_buf();
        let delete_set = delete_set.clone();
        let safe_flags = try_par_map(&needs_walk, options, 0, |dir| {
            Ok(wholesale_directory_safe_to_remove(
                &wt,
                dir,
                &delete_set,
                ignorecase,
            ))
        })
        .unwrap_or_else(|_: ParallelHashError<()>| {
            needs_walk
                .iter()
                .map(|dir| {
                    wholesale_directory_safe_to_remove(work_tree, dir, &delete_set, ignorecase)
                })
                .collect()
        });
        for (dir, safe) in needs_walk.into_iter().zip(safe_flags) {
            if safe {
                trusted.insert(dir);
            }
        }
    } else {
        for dir in needs_walk {
            if wholesale_directory_safe_to_remove(work_tree, &dir, &delete_set, ignorecase) {
                trusted.insert(dir);
            }
        }
    }
    filter_wholesale_dirs_with_populated_gitlinks(work_tree, trusted, gitlink_deletes)
}

pub(crate) fn path_under_wholesale_directory(
    path: &str,
    wholesale_dirs: &BTreeSet<String>,
) -> bool {
    wholesale_dirs.iter().any(|dir| {
        path == dir.as_str()
            || path
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn checkout_stored_worktree_abs(work_tree: &Path, stored_path: &str) -> PathBuf {
    crate::repo_relpath::worktree_abs(work_tree, stored_path)
}

fn checkout_stored_index_path_bytes(stored_path: &str) -> Vec<u8> {
    crate::repo_relpath::storage_bytes(stored_path).into_owned()
}

const BULK_ADD_CHECKOUT_MIN: usize = 32;
/// Change count from which a checkout's index write skips the pre-write housekeeping.
const BULK_INDEX_REWRITE_MIN: usize = 64;
const BULK_DELETE_CHECKOUT_MIN: usize = 32;

/// Remove a wholesale-deleted directory; true once it no longer exists. A directory that only
/// held skip-worktree paths is usually absent already.
fn remove_wholesale_dir(abs: &Path) -> bool {
    match fs::remove_dir_all(abs) {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

fn remove_wholesale_directories_on_disk(
    repo: &Repository,
    work_tree: &Path,
    wholesale_index_prefixes: &BTreeSet<String>,
    config: &std::sync::Arc<ConfigSet>,
) -> Result<BTreeSet<String>> {
    let mut wholesale_dirs_removed = BTreeSet::new();
    if wholesale_index_prefixes.is_empty() {
        return Ok(wholesale_dirs_removed);
    }
    if wholesale_index_prefixes.len() >= 2 {
        let policy = repo.thread_policy();
        let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
        let wt = work_tree.to_path_buf();
        let dirs: Vec<String> = wholesale_index_prefixes.iter().cloned().collect();
        let removed = try_par_map(&dirs, options, 0, |dir| {
            let abs = wt.join(dir);
            Ok(remove_wholesale_dir(&abs).then_some(dir.clone()))
        })
        .map_err(|e: ParallelHashError<()>| match e {
            ParallelHashError::Task(()) => {
                Error::PathError("parallel wholesale directory removal failed".into())
            }
        })?;
        wholesale_dirs_removed.extend(removed.into_iter().flatten());
    } else {
        for dir in wholesale_index_prefixes {
            let abs = work_tree.join(dir);
            if remove_wholesale_dir(&abs) {
                wholesale_dirs_removed.insert(dir.clone());
            }
        }
    }
    Ok(wholesale_dirs_removed)
}

#[allow(clippy::too_many_arguments)]
fn checkout_commit_index_update(
    repo: &Repository,
    index_mut: &mut Index,
    wholesale_dirs_removed: &BTreeSet<String>,
    paths_to_remove: &[Vec<u8>],
    mut new_entries: Vec<IndexEntry>,
    skip_worktree_rows: &[IndexEntry],
    bulk_index_rewrite: bool,
    checkout_result_tree: Option<crate::objects::ObjectId>,
    defer_index_disk_write: bool,
) -> Result<()> {
    new_entries.extend_from_slice(skip_worktree_rows);
    let change_count = paths_to_remove.len() + new_entries.len();
    let pre_mtime_max = crate::index::stage0_mtime_max(&index_mut.entries);
    let new_mtime_max = crate::index::stage0_mtime_max(&new_entries);
    let index_mtime_hint = Some(crate::index::max_stage0_mtime_pair(
        pre_mtime_max,
        new_mtime_max,
    ));
    index_mut.defer_untracked_cache_invalidation =
        bulk_index_rewrite || !wholesale_dirs_removed.is_empty();
    if bulk_index_rewrite || !wholesale_dirs_removed.is_empty() {
        if index_mut.defer_untracked_cache_invalidation {
            const CACHE_TREE_PARTIAL_INVALIDATE_MAX: usize = 4096;
            if !wholesale_dirs_removed.is_empty() {
                index_mut.cache_tree = None;
                index_mut.cache_tree_root = None;
            } else if change_count <= CACHE_TREE_PARTIAL_INVALIDATE_MAX {
                index_mut.invalidate_cache_tree_for_bulk_checkout(
                    wholesale_dirs_removed,
                    paths_to_remove.iter().map(|p| p.as_slice()),
                    &new_entries,
                );
            } else {
                index_mut.cache_tree = None;
                index_mut.cache_tree_root = None;
            }
        } else {
            index_mut.drop_derived_index_extensions();
        }
    }
    index_mut.remove_prefixes_paths_and_insert(
        wholesale_dirs_removed,
        paths_to_remove.iter().map(|p| p.as_slice()),
        new_entries,
    );
    index_mut.ensure_canonical_order();
    if let Some(tree) = checkout_result_tree {
        attach_checkout_result_cache_tree(repo, index_mut, &tree)?;
    }
    index_mut.defer_untracked_cache_invalidation = false;
    if defer_index_disk_write {
        return Ok(());
    }
    write_checkout_index(repo, index_mut, bulk_index_rewrite, index_mtime_hint)
}

/// Record that `index_mut` now matches `tree`: keep a still-valid cache tree and point its
/// root at `tree`, or build one from `tree` when the entry counts agree.
fn attach_checkout_result_cache_tree(
    repo: &Repository,
    index_mut: &mut Index,
    tree: &ObjectId,
) -> Result<()> {
    if index_mut.cache_tree.is_some() {
        index_mut.cache_tree_root = Some(*tree);
        return Ok(());
    }
    if index_mut.has_sparse_directory_placeholders() {
        let target = crate::porcelain::index_from_tree::index_from_tree_reusing_cache_tree(
            repo, tree, index_mut,
        )?;
        if let Some(cache_tree) = target
            .cache_tree
            .filter(|_| target.entries.len() == index_mut.entries.len())
        {
            index_mut.set_cache_tree(cache_tree);
        }
    } else {
        let cache_tree = crate::write_tree::build_cache_tree_from_tree(repo.odb(), tree)?;
        if usize::try_from(cache_tree.entry_count).ok() == Some(index_mut.entries.len()) {
            index_mut.set_cache_tree(cache_tree);
        }
    }
    Ok(())
}

/// Write the index after a checkout, with fresh stat data trusted (no racy smudging), then
/// move the index file mtime past the newest entry so later status checks stay fast.
fn write_checkout_index(
    repo: &Repository,
    index_mut: &mut Index,
    bulk_index_rewrite: bool,
    index_mtime_hint: Option<(u32, u32)>,
) -> Result<()> {
    let index_path = repo.index_path_for_env()?;
    index_mut.suppress_pre_write_index_housekeeping = bulk_index_rewrite;
    index_mut.skip_racy_smudge_on_write = true;
    let result = repo.write_index_at_with_post_index_change(&index_path, index_mut, true, false);
    index_mut.skip_racy_smudge_on_write = false;
    index_mut.suppress_pre_write_index_housekeeping = false;
    result?;
    crate::index::advance_index_file_mtime_past_entries(index_mut, &index_path, index_mtime_hint)?;
    index_mut.source_mtime = crate::index::index_file_mtime(&index_path);
    Ok(())
}

/// Split off the changes that sparse checkout keeps out of the work tree.
///
/// Those paths get no work-tree I/O: deleted ones lose their index row here, and the others get
/// a row in [`CheckoutApplyOptions::skip_worktree_rows`], which the index commit merges after any
/// wholesale directory removal. The row has `skip-worktree` set unless the path is new to the
/// index and a file already sits there: that file is left untouched and tracked, like Git.
/// Returns the changes that still have to be applied to the work tree; when sparse checkout is
/// off this is `changes` itself.
fn apply_sparse_tree_changes_to_index<'a>(
    sparse: Option<&SparseCheckout>,
    work_tree: &Path,
    index: &mut Index,
    changes: &'a [DiffEntry],
    apply: &mut CheckoutApplyOptions,
) -> Cow<'a, [DiffEntry]> {
    let Some(sparse) = sparse else {
        return Cow::Borrowed(changes);
    };
    apply.skip_worktree_rows.clear();
    let skipped_path = |change: &'a DiffEntry| -> Option<&'a str> {
        let path = match change.status {
            DiffStatus::Deleted => change.old_path.as_deref(),
            DiffStatus::Added | DiffStatus::Modified | DiffStatus::TypeChanged => {
                change.new_path.as_deref()
            }
            _ => None,
        }?;
        sparse.checkout_skips(index, path).then_some(path)
    };
    let skipped: Vec<Option<&str>> = changes.iter().map(skipped_path).collect();
    if skipped.iter().all(Option::is_none) {
        return Cow::Borrowed(changes);
    }
    let mut worktree = Vec::with_capacity(changes.len());
    let mut removed: Vec<Vec<u8>> = Vec::new();
    for (change, path) in changes.iter().zip(skipped) {
        let Some(path) = path else {
            worktree.push(change.clone());
            continue;
        };
        if change.status == DiffStatus::Deleted {
            removed.push(checkout_stored_index_path_bytes(path));
            continue;
        }
        let mut entry =
            prospective_index_entry(path, change.new_oid, parse_git_mode(&change.new_mode));
        entry.path = checkout_stored_index_path_bytes(path);
        let untracked_file_present = index.get(&entry.path, 0).is_none()
            && fs::symlink_metadata(checkout_stored_worktree_abs(work_tree, path)).is_ok();
        entry.set_skip_worktree(!untracked_file_present);
        apply.skip_worktree_rows.push(entry);
    }
    if !removed.is_empty() {
        index.remove_prefixes_paths_and_insert(
            &BTreeSet::new(),
            removed.iter().map(Vec::as_slice),
            Vec::new(),
        );
    }
    Cow::Owned(worktree)
}

fn tree_changes_are_all_additions(changes: &[DiffEntry]) -> bool {
    !changes.is_empty() && changes.iter().all(|c| c.status == DiffStatus::Added)
}

fn tree_changes_are_all_modifications(changes: &[DiffEntry]) -> bool {
    !changes.is_empty() && changes.iter().all(|c| c.status == DiffStatus::Modified)
}

fn tree_changes_are_all_deletions(changes: &[DiffEntry]) -> bool {
    !changes.is_empty() && changes.iter().all(|c| c.status == DiffStatus::Deleted)
}

fn tree_changes_are_bulk_mod_add_delete(changes: &[DiffEntry]) -> bool {
    !changes.is_empty()
        && changes.iter().all(|c| {
            matches!(
                c.status,
                DiffStatus::Modified | DiffStatus::Deleted | DiffStatus::Added
            )
        })
}

fn bulk_write_checkout_eligible(changes: &[DiffEntry], rules: &WorktreeRules) -> bool {
    changes.len() >= BULK_ADD_CHECKOUT_MIN
        && changes
            .iter()
            .all(|c| matches!(c.status, DiffStatus::Added | DiffStatus::Modified))
        && changes.iter().all(|change| {
            let Some(path) = change.new_path.as_deref() else {
                return false;
            };
            let mode = parse_git_mode(&change.new_mode);
            mode != MODE_GITLINK
                && is_eligible_for_parallel_checkout(mode, &rules.file_attrs(path, false))
        })
}

fn bulk_mod_delete_checkout_eligible(changes: &[DiffEntry], rules: &WorktreeRules) -> bool {
    if changes.len() < BULK_ADD_CHECKOUT_MIN
        || !tree_changes_are_bulk_mod_add_delete(changes)
        || tree_changes_touch_gitattributes(changes)
    {
        return false;
    }
    let mut saw_write = false;
    for change in changes {
        if !matches!(change.status, DiffStatus::Modified | DiffStatus::Added) {
            continue;
        }
        saw_write = true;
        let Some(path) = change.new_path.as_deref() else {
            return false;
        };
        let mode = parse_git_mode(&change.new_mode);
        if mode == MODE_GITLINK
            || !is_eligible_for_parallel_checkout(mode, &rules.file_attrs(path, false))
        {
            return false;
        }
    }
    saw_write
}

#[allow(clippy::too_many_arguments)]
fn checkout_bulk_added_tree_changes(
    repo: &Repository,
    work_tree: &Path,
    changes: &[DiffEntry],
    index_mut: &mut Index,
    rules: &WorktreeRules,
    config: &std::sync::Arc<ConfigSet>,
    conv: &crlf::ConversionConfig,
    apply: CheckoutApplyOptions,
    precompose: bool,
    ignorecase: bool,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    let checkout_smudge_ctx = crate::filter_process::checkout_smudge_context(repo);
    let mut dir_cache = LeadingDirCache::with_protect(ProtectOptions::from_config(config.as_ref()));
    let mut write_jobs: Vec<CheckoutWriteJob<'_>> = Vec::with_capacity(changes.len());
    for change in changes {
        crate::progress::poll_cancel(progress, cancel)?;
        let Some(path) = change.new_path.as_deref() else {
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        let file_attrs = rules.file_attrs(path, false);
        write_jobs.push(CheckoutWriteJob {
            path: path.to_owned(),
            mode,
            oid: change.new_oid,
            change,
            file_attrs,
            smudge_meta: crate::filter_process::smudge_meta_for_checkout_blob(
                &checkout_smudge_ctx,
                &change.new_oid.to_hex(),
            ),
        });
    }
    let write_paths: Vec<String> = write_jobs.iter().map(|j| j.path.clone()).collect();
    let serial_paths = paths_needing_serial_checkout(&write_paths, ignorecase, precompose);
    let parallel_indices: HashSet<usize> = write_jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| {
            is_eligible_for_parallel_checkout(job.mode, &job.file_attrs)
                && !serial_paths.contains(&job.path)
        })
        .map(|(i, _)| i)
        .collect();
    if write_jobs.len() >= BULK_ADD_CHECKOUT_MIN {
        precreate_checkout_parent_dirs(work_tree, &write_jobs, &mut dir_cache);
    } else {
        for job in &write_jobs {
            dir_cache.ensure_parents(work_tree, &job.path)?;
        }
    }
    let mut delayed = DelayedProcessCheckout::default();
    let write_outcomes = execute_write_jobs(
        repo,
        rules,
        config,
        conv,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        &write_jobs,
        &parallel_indices,
        apply.parallel_prepare_blobs,
        apply.checkout_parallel_threshold,
        apply.skip_parallel_write_ramp,
        progress,
        cancel,
    )?;
    let new_entries = finish_bulk_write_jobs(
        repo,
        rules,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        write_outcomes,
    )?;
    if apply.wholesale_safety_precomputed {
        let _ = remove_wholesale_directories_on_disk(
            repo,
            work_tree,
            &apply.preverified_wholesale_dirs,
            config,
        )?;
    }
    checkout_commit_index_update(
        repo,
        index_mut,
        &apply.preverified_wholesale_dirs,
        &[],
        new_entries,
        &apply.skip_worktree_rows,
        true,
        apply.checkout_result_tree,
        apply.defer_index_disk_write,
    )?;
    progress.finish();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn checkout_bulk_mod_delete_tree_changes(
    repo: &Repository,
    work_tree: &Path,
    changes: &[DiffEntry],
    index_mut: &mut Index,
    rules: &WorktreeRules,
    config: &std::sync::Arc<ConfigSet>,
    conv: &crlf::ConversionConfig,
    apply: CheckoutApplyOptions,
    precompose: bool,
    ignorecase: bool,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    delete_rollback: SharedCheckoutDeleteRollback,
) -> Result<()> {
    let index: &Index = index_mut;
    let delete_changes: Vec<&DiffEntry> = changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .collect();
    let write_changes = changes
        .iter()
        .filter(|c| matches!(c.status, DiffStatus::Modified | DiffStatus::Added));
    let delete_paths: Vec<String> = delete_changes
        .iter()
        .filter_map(|c| c.old_path.clone())
        .collect();
    let gitlink_deletes: HashSet<String> = delete_changes
        .iter()
        .filter_map(|c| {
            let path = c.old_path.as_ref()?;
            (parse_git_mode(&c.old_mode) == MODE_GITLINK).then(|| path.clone())
        })
        .collect();
    let owned_wholesale_candidates;
    let wholesale_index_prefixes: &BTreeSet<String> = if apply.wholesale_safety_precomputed {
        &apply.preverified_wholesale_dirs
    } else {
        owned_wholesale_candidates = wholesale_deleted_directories_on_disk(
            repo,
            work_tree,
            index,
            &delete_paths,
            ignorecase,
            &apply,
            &gitlink_deletes,
        );
        &owned_wholesale_candidates
    };

    let checkout_smudge_ctx = crate::filter_process::checkout_smudge_context(repo);
    let mut dir_cache = LeadingDirCache::with_protect(ProtectOptions::from_config(config.as_ref()));
    let mut write_jobs: Vec<CheckoutWriteJob<'_>> =
        Vec::with_capacity(changes.len() - delete_changes.len());
    for change in write_changes {
        let Some(path) = change.new_path.as_deref() else {
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        let file_attrs = rules.file_attrs(path, false);
        write_jobs.push(CheckoutWriteJob {
            path: path.to_owned(),
            mode,
            oid: change.new_oid,
            change,
            file_attrs,
            smudge_meta: crate::filter_process::smudge_meta_for_checkout_blob(
                &checkout_smudge_ctx,
                &change.new_oid.to_hex(),
            ),
        });
    }

    let wholesale_dirs_removed =
        remove_wholesale_directories_on_disk(repo, work_tree, wholesale_index_prefixes, config)?;
    let mut deleted_abs_paths: Vec<PathBuf> = wholesale_dirs_removed
        .iter()
        .map(|dir| work_tree.join(dir))
        .collect();
    let mut per_file_deletes: Vec<String> = Vec::new();
    for change in &delete_changes {
        let Some(path) = &change.old_path else {
            continue;
        };
        if parse_git_mode(&change.old_mode) == MODE_GITLINK
            || path_under_wholesale_directory(path, wholesale_index_prefixes)
        {
            continue;
        }
        per_file_deletes.push(path.clone());
    }
    let mut parallel_deleted_paths: HashSet<String> = HashSet::new();
    if per_file_deletes.len() >= PARALLEL_PER_FILE_DELETE_MIN {
        let policy = repo.thread_policy();
        let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
        let (parallel_set, abs_paths) = remove_checkout_files_parallel_cancellable(
            work_tree,
            &per_file_deletes,
            options,
            progress,
            cancel,
            delete_rollback.clone_handle(),
        )?;
        deleted_abs_paths.extend(abs_paths);
        parallel_deleted_paths.extend(parallel_set);
    }
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(delete_changes.len());
    for change in &delete_changes {
        crate::progress::poll_cancel(progress, cancel)?;
        let Some(path) = &change.old_path else {
            continue;
        };
        let old_mode = parse_git_mode(&change.old_mode);
        if old_mode == MODE_GITLINK
            || !path_under_wholesale_directory(path, &wholesale_dirs_removed)
        {
            if old_mode == MODE_GITLINK {
                let abs = checkout_stored_worktree_abs(work_tree, path);
                remove_uninitialized_gitlink_worktree(&abs);
                deleted_abs_paths.push(abs);
            } else if !parallel_deleted_paths.contains(path) {
                remove_checkout_worktree_file(work_tree, path, &delete_rollback)?;
                deleted_abs_paths.push(checkout_stored_worktree_abs(work_tree, path));
            }
        }
        if !path_under_wholesale_directory(path, wholesale_index_prefixes) {
            paths_to_remove.push(checkout_stored_index_path_bytes(path));
        }
        if !parallel_deleted_paths.contains(path) {
            progress.inc(1);
            crate::progress::poll_cancel(progress, cancel)?;
        }
    }
    remove_empty_parent_dirs_batch(work_tree, &deleted_abs_paths);

    let write_paths: Vec<String> = write_jobs.iter().map(|j| j.path.clone()).collect();
    let serial_paths = paths_needing_serial_checkout(&write_paths, ignorecase, precompose);
    let parallel_indices: HashSet<usize> = write_jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| {
            is_eligible_for_parallel_checkout(job.mode, &job.file_attrs)
                && !serial_paths.contains(&job.path)
        })
        .map(|(i, _)| i)
        .collect();
    if write_jobs.len() >= BULK_ADD_CHECKOUT_MIN {
        precreate_checkout_parent_dirs(work_tree, &write_jobs, &mut dir_cache);
    } else {
        for job in &write_jobs {
            dir_cache.ensure_parents(work_tree, &job.path)?;
        }
    }
    let mut delayed = DelayedProcessCheckout::default();
    let write_outcomes = execute_write_jobs(
        repo,
        rules,
        config,
        conv,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        &write_jobs,
        &parallel_indices,
        apply.parallel_prepare_blobs,
        apply.checkout_parallel_threshold,
        apply.skip_parallel_write_ramp,
        progress,
        cancel,
    )?;
    let new_entries = finish_bulk_write_jobs(
        repo,
        rules,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        write_outcomes,
    )?;

    checkout_commit_index_update(
        repo,
        index_mut,
        wholesale_index_prefixes,
        &paths_to_remove,
        new_entries,
        &apply.skip_worktree_rows,
        true,
        apply.checkout_result_tree,
        apply.defer_index_disk_write,
    )?;
    progress.finish();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn checkout_bulk_deleted_tree_changes(
    repo: &Repository,
    work_tree: &Path,
    changes: &[DiffEntry],
    index_mut: &mut Index,
    wholesale_index_prefixes: &BTreeSet<String>,
    wholesale_dirs_removed: &BTreeSet<String>,
    deleted_abs_paths: &mut Vec<PathBuf>,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    apply: &CheckoutApplyOptions,
    delete_rollback: SharedCheckoutDeleteRollback,
) -> Result<()> {
    let mut parallel_deleted_paths: HashSet<String> = HashSet::new();
    let mut per_file_deletes: Vec<String> = Vec::new();
    for change in changes {
        crate::progress::poll_cancel(progress, cancel)?;
        if change.status != DiffStatus::Deleted {
            continue;
        }
        let Some(path) = &change.old_path else {
            continue;
        };
        let old_mode = parse_git_mode(&change.old_mode);
        if old_mode == MODE_GITLINK || path_under_wholesale_directory(path, wholesale_dirs_removed)
        {
            continue;
        }
        per_file_deletes.push(path.clone());
    }
    if per_file_deletes.len() >= PARALLEL_PER_FILE_DELETE_MIN {
        let config = repo.config()?;
        let policy = repo.thread_policy();
        let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
        let (parallel_set, abs_paths) = remove_checkout_files_parallel_cancellable(
            work_tree,
            &per_file_deletes,
            options,
            progress,
            cancel,
            delete_rollback.clone_handle(),
        )?;
        deleted_abs_paths.extend(abs_paths);
        parallel_deleted_paths.extend(parallel_set);
    }
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(changes.len());
    for change in changes {
        crate::progress::poll_cancel(progress, cancel)?;
        if change.status != DiffStatus::Deleted {
            continue;
        }
        let Some(path) = &change.old_path else {
            continue;
        };
        let old_mode = parse_git_mode(&change.old_mode);
        if old_mode == MODE_GITLINK || !path_under_wholesale_directory(path, wholesale_dirs_removed)
        {
            if old_mode == MODE_GITLINK {
                let abs = checkout_stored_worktree_abs(work_tree, path);
                remove_uninitialized_gitlink_worktree(&abs);
                deleted_abs_paths.push(abs);
            } else if !parallel_deleted_paths.contains(path) {
                remove_checkout_worktree_file(work_tree, path, &delete_rollback)?;
                deleted_abs_paths.push(checkout_stored_worktree_abs(work_tree, path));
            }
        }
        if !path_under_wholesale_directory(path, wholesale_index_prefixes) {
            paths_to_remove.push(checkout_stored_index_path_bytes(path));
        }
        if !parallel_deleted_paths.contains(path) {
            progress.inc(1);
            crate::progress::poll_cancel(progress, cancel)?;
        }
    }
    remove_empty_parent_dirs_batch(work_tree, deleted_abs_paths);
    checkout_commit_index_update(
        repo,
        index_mut,
        wholesale_index_prefixes,
        &paths_to_remove,
        Vec::new(),
        &apply.skip_worktree_rows,
        true,
        apply.checkout_result_tree,
        apply.defer_index_disk_write,
    )?;
    progress.finish();
    Ok(())
}

#[cfg(test)]
thread_local! {
    static PREFETCH_PROMISOR_FOR_CHECKOUT_CALLS: std::cell::RefCell<usize> =
        const { std::cell::RefCell::new(0) };
}

fn prefetch_promisor_blobs_for_checkout(repo: &Repository, changes: &[DiffEntry]) -> Result<()> {
    #[cfg(test)]
    PREFETCH_PROMISOR_FOR_CHECKOUT_CALLS.with(|c| *c.borrow_mut() += 1);
    let oids = changes.iter().filter_map(|change| {
        if change.status == DiffStatus::Deleted {
            return None;
        }
        let mode = parse_git_mode(&change.new_mode);
        matches!(mode, MODE_REGULAR | MODE_EXECUTABLE | MODE_SYMLINK).then_some(change.new_oid)
    });
    prefetch_promisor_blobs(repo, oids)
}

/// Prefetch only missing, distinct blobs, retaining lazy-read behavior when disabled.
fn prefetch_promisor_blobs(
    repo: &Repository,
    oids: impl IntoIterator<Item = ObjectId>,
) -> Result<()> {
    let config = repo.config()?;
    if !crate::promisor::repo_treats_promisor_packs(repo.git_dir(), config.as_ref())
        || !crate::promisor_fetch::promisor_lazy_fetch_enabled(config.as_ref(), repo.environment())
        || repo.caches().promisor_fetcher().is_none()
    {
        return Ok(());
    }
    let unique: crate::objects::OidHashSet =
        oids.into_iter().filter(|oid| !oid.is_zero()).collect();
    let missing = repo.odb().missing_stored_oids(unique)?;
    if missing.is_empty() {
        return Ok(());
    }
    repo.prefetch_promised(&missing)
}

fn checkout_tree_changes_with_progress(
    repo: &Repository,
    changes: &[DiffEntry],
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
    reuse_index: Option<&mut Index>,
    apply: CheckoutApplyOptions,
    attribute_changes: Option<&[DiffEntry]>,
) -> Result<()> {
    let mut progress = CancelProgress::new(progress, cancel);
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;
    let mut delete_guard = CheckoutDeleteGuard::new(work_tree.to_path_buf())?;
    let delete_rollback = delete_guard.shared();

    let mut loaded_index;
    let index_mut: &mut Index = if let Some(index) = reuse_index {
        index
    } else {
        loaded_index = repo.load_index()?;
        &mut loaded_index
    };
    let config = repo.config()?;
    let protect = ProtectOptions::from_config(config.as_ref());
    validate_diff_changes_for_worktree(changes, protect)?;
    let mut apply = apply;
    let sparse = if apply.sparse_paths_applied {
        None
    } else {
        SparseCheckout::load(repo)?
    };
    let worktree_changes = apply_sparse_tree_changes_to_index(
        sparse.as_ref(),
        work_tree,
        index_mut,
        changes,
        &mut apply,
    );
    let changes: &[DiffEntry] = &worktree_changes;

    prefetch_promisor_blobs_for_checkout(repo, changes)?;
    if let Some(extra) = attribute_changes {
        validate_diff_changes_for_worktree(extra, protect)?;
    }
    let ignorecase = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let precompose = repo.pathspec_precompose_enabled();
    let attr_changes = attribute_changes.unwrap_or(changes);

    progress.start(
        ProgressPhase::CheckingOutFiles,
        Some(u64::try_from(changes.len()).unwrap_or(u64::MAX)),
    );

    if !apply.skip_bulk_checkout_fast_path {
        let precomputed_rules = apply.precomputed_worktree_rules.clone();
        let owned_bulk_rules;
        let bulk_rules: &WorktreeRules = if let Some(ref precomputed) = precomputed_rules {
            precomputed.as_ref()
        } else {
            owned_bulk_rules =
                checkout_worktree_rules(repo, index_mut, attr_changes, config.clone())?;
            &owned_bulk_rules
        };
        let conv = bulk_rules.conversion().clone();

        if bulk_mod_delete_checkout_eligible(changes, bulk_rules) {
            checkout_bulk_mod_delete_tree_changes(
                repo,
                work_tree,
                changes,
                index_mut,
                bulk_rules,
                &config,
                &conv,
                apply,
                precompose,
                ignorecase,
                &mut progress,
                cancel,
                delete_rollback.clone_handle(),
            )?;
            delete_guard.commit_success();
            return progress.finish_poll();
        }
        if bulk_write_checkout_eligible(changes, bulk_rules)
            && (tree_changes_are_all_additions(changes)
                || tree_changes_are_all_modifications(changes))
        {
            checkout_bulk_added_tree_changes(
                repo,
                work_tree,
                changes,
                index_mut,
                bulk_rules,
                &config,
                &conv,
                apply,
                precompose,
                ignorecase,
                &mut progress,
                cancel,
            )?;
            delete_guard.commit_success();
            return progress.finish_poll();
        }
        if changes.len() >= BULK_DELETE_CHECKOUT_MIN && tree_changes_are_all_deletions(changes) {
            let index: &Index = index_mut;
            let delete_paths: Vec<String> = changes
                .iter()
                .filter(|c| c.status == DiffStatus::Deleted)
                .filter_map(|c| c.old_path.clone())
                .collect();
            let gitlink_deletes: HashSet<String> = changes
                .iter()
                .filter(|c| c.status == DiffStatus::Deleted)
                .filter_map(|c| {
                    let path = c.old_path.as_ref()?;
                    (parse_git_mode(&c.old_mode) == MODE_GITLINK).then(|| path.clone())
                })
                .collect();
            let wholesale_candidates = if apply.wholesale_safety_precomputed {
                apply.preverified_wholesale_dirs.clone()
            } else {
                wholesale_deleted_directories_on_disk(
                    repo,
                    work_tree,
                    index,
                    &delete_paths,
                    ignorecase,
                    &apply,
                    &gitlink_deletes,
                )
            };
            let wholesale_index_prefixes = wholesale_candidates;
            let mut wholesale_dirs_removed = BTreeSet::new();
            let mut deleted_abs_paths: Vec<PathBuf> = Vec::new();
            if !wholesale_index_prefixes.is_empty() {
                let policy = repo.thread_policy();
                let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
                let dirs: Vec<String> = wholesale_index_prefixes.iter().cloned().collect();
                for (dir, abs) in remove_wholesale_dirs_parallel_cancellable(
                    work_tree,
                    &dirs,
                    options,
                    &mut progress,
                    cancel,
                )? {
                    wholesale_dirs_removed.insert(dir);
                    deleted_abs_paths.push(abs);
                }
            }
            checkout_bulk_deleted_tree_changes(
                repo,
                work_tree,
                changes,
                index_mut,
                &wholesale_index_prefixes,
                &wholesale_dirs_removed,
                &mut deleted_abs_paths,
                &mut progress,
                cancel,
                &apply,
                delete_rollback.clone_handle(),
            )?;
            delete_guard.commit_success();
            return progress.finish_poll();
        }
    }

    let index: &Index = index_mut;
    let checkout_smudge_ctx = crate::filter_process::checkout_smudge_context(repo);
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(changes.len() / 8);
    let mut deleted_abs_paths: Vec<PathBuf> = Vec::with_capacity(changes.len() / 8);
    let mut dir_cache = LeadingDirCache::with_protect(protect);
    let mut plan: Vec<CheckoutPlanSlot> = Vec::with_capacity(changes.len());
    let mut write_jobs: Vec<CheckoutWriteJob<'_>> = Vec::new();
    let mut rules: Option<Rc<WorktreeRules>> =
        apply.precomputed_worktree_rules.as_ref().map(Rc::clone);
    let mut conv: Option<ConversionConfig> = None;
    let mut delayed = DelayedProcessCheckout::default();
    let mut pending_delayed: Vec<DelayedCheckoutTarget> = Vec::new();
    let attr_changes = attribute_changes.unwrap_or(changes);
    let precompose = repo.pathspec_precompose_enabled();

    let delete_paths: Vec<String> = changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .filter_map(|c| c.old_path.clone())
        .collect();
    let gitlink_deletes: HashSet<String> = changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .filter_map(|c| {
            let path = c.old_path.as_ref()?;
            (parse_git_mode(&c.old_mode) == MODE_GITLINK).then(|| path.clone())
        })
        .collect();
    let wholesale_candidates = if apply.wholesale_safety_precomputed {
        apply.preverified_wholesale_dirs.clone()
    } else {
        wholesale_deleted_directories_on_disk(
            repo,
            work_tree,
            index,
            &delete_paths,
            ignorecase,
            &apply,
            &gitlink_deletes,
        )
    };
    let wholesale_index_prefixes = wholesale_candidates;
    let mut wholesale_dirs_removed = BTreeSet::new();
    if !wholesale_index_prefixes.is_empty() {
        let policy = repo.thread_policy();
        let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
        let dirs: Vec<String> = wholesale_index_prefixes.iter().cloned().collect();
        for (dir, abs) in remove_wholesale_dirs_parallel_cancellable(
            work_tree,
            &dirs,
            options,
            &mut progress,
            cancel,
        )? {
            wholesale_dirs_removed.insert(dir);
            deleted_abs_paths.push(abs);
        }
    }

    let mut parallel_deleted_paths: HashSet<String> = HashSet::new();
    let mut per_file_deletes: Vec<String> = Vec::new();
    for change in changes {
        if change.status != DiffStatus::Deleted {
            continue;
        }
        let Some(path) = &change.old_path else {
            continue;
        };
        let old_mode = parse_git_mode(&change.old_mode);
        if old_mode == MODE_GITLINK || path_under_wholesale_directory(path, &wholesale_dirs_removed)
        {
            continue;
        }
        per_file_deletes.push(path.clone());
    }
    if per_file_deletes.len() >= PARALLEL_PER_FILE_DELETE_MIN {
        let policy = repo.thread_policy();
        let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
        let (parallel_set, abs_paths) = remove_checkout_files_parallel_cancellable(
            work_tree,
            &per_file_deletes,
            options,
            &mut progress,
            cancel,
            delete_rollback.clone_handle(),
        )?;
        deleted_abs_paths.extend(abs_paths);
        parallel_deleted_paths.extend(parallel_set);
    }

    {
        crate::trace::trace_span!("grit.checkout.plan");
        for change in changes {
            if change.status == DiffStatus::Deleted {
                crate::progress::poll_cancel(&mut progress, cancel)?;
                if let Some(path) = &change.old_path {
                    let old_mode = parse_git_mode(&change.old_mode);
                    if old_mode == MODE_GITLINK
                        || !path_under_wholesale_directory(path, &wholesale_dirs_removed)
                    {
                        if old_mode == MODE_GITLINK {
                            let abs = checkout_stored_worktree_abs(work_tree, path);
                            remove_uninitialized_gitlink_worktree(&abs);
                            deleted_abs_paths.push(abs);
                        } else if !parallel_deleted_paths.contains(path) {
                            remove_checkout_worktree_file(work_tree, path, &delete_rollback)?;
                            deleted_abs_paths.push(checkout_stored_worktree_abs(work_tree, path));
                        }
                    }
                    if !path_under_wholesale_directory(path, &wholesale_dirs_removed) {
                        paths_to_remove.push(checkout_stored_index_path_bytes(path));
                    }
                }
                if change
                    .old_path
                    .as_ref()
                    .is_none_or(|path| !parallel_deleted_paths.contains(path))
                {
                    progress.inc(1);
                    crate::progress::poll_cancel(&mut progress, cancel)?;
                }
                continue;
            }

            let Some(path) = &change.new_path else {
                progress.inc(1);
                continue;
            };
            if rules.is_none() {
                rules = Some(Rc::new(checkout_worktree_rules(
                    repo,
                    index,
                    attr_changes,
                    config.clone(),
                )?));
            }
            let rules = rules.as_deref().ok_or_else(|| {
                Error::PathError("internal error: missing checkout rules for write".into())
            })?;
            if conv.is_none() {
                conv = Some(rules.conversion().clone());
            }
            let mode = parse_git_mode(&change.new_mode);
            if mode == MODE_GITLINK {
                let index_path = checkout_stored_index_path_bytes(path);
                let abs = checkout_stored_worktree_abs(work_tree, path);
                if let Some(existing) = index.get(&index_path, 0) {
                    if existing.oid == change.new_oid
                        && existing.mode == MODE_GITLINK
                        && gitlink_worktree_matches_index(&abs)
                    {
                        let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                        let mut entry =
                            entry_from_metadata(&meta, &index_path, change.new_oid, MODE_GITLINK);
                        entry.size = 0;
                        plan.push(CheckoutPlanSlot::Ready(entry));
                        progress.inc(1);
                        continue;
                    }
                }
                materialize_gitlink_worktree(work_tree, path, &mut dir_cache)?;
                let mut entry = entry_from_stat(&abs, &index_path, change.new_oid, MODE_GITLINK)?;
                entry.size = 0;
                plan.push(CheckoutPlanSlot::Ready(entry));
                progress.inc(1);
                continue;
            }
            let index_path = checkout_stored_index_path_bytes(path);
            let abs = checkout_stored_worktree_abs(work_tree, path);
            if let Some(existing) = index.get(&index_path, 0) {
                if crate::diff::path_checkout_skip_blob_write_when_up_to_date_with_config(
                    repo.odb(),
                    repo.git_dir(),
                    work_tree,
                    index,
                    existing,
                    &change.new_oid,
                    mode,
                    path,
                    Some(rules.filter_process()),
                    Some(config.as_ref()),
                )? {
                    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                    plan.push(CheckoutPlanSlot::Ready(entry_from_metadata(
                        &meta,
                        &index_path,
                        change.new_oid,
                        mode,
                    )));
                    progress.inc(1);
                    continue;
                }
            }
            let file_attrs = rules.file_attrs(path, false);
            let smudge_meta = crate::filter_process::smudge_meta_for_checkout_blob(
                &checkout_smudge_ctx,
                &change.new_oid.to_hex(),
            );
            write_jobs.push(CheckoutWriteJob {
                path: path.clone(),
                mode,
                oid: change.new_oid,
                change,
                file_attrs,
                smudge_meta,
            });
            plan.push(CheckoutPlanSlot::Write);
        }
    }

    let write_paths: Vec<String> = write_jobs.iter().map(|j| j.path.clone()).collect();
    let serial_paths = paths_needing_serial_checkout(&write_paths, ignorecase, precompose);
    let parallel_indices: HashSet<usize> = write_jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| {
            is_eligible_for_parallel_checkout(job.mode, &job.file_attrs)
                && !serial_paths.contains(&job.path)
        })
        .map(|(i, _)| i)
        .collect();

    if write_jobs.len() >= 64 {
        precreate_checkout_parent_dirs(work_tree, &write_jobs, &mut dir_cache);
    } else {
        for job in &write_jobs {
            dir_cache.ensure_parents(work_tree, &job.path)?;
        }
    }

    let write_outcomes = if write_jobs.is_empty() {
        Vec::new()
    } else {
        let rules = rules.as_ref().ok_or_else(|| {
            Error::PathError("internal error: missing checkout rules for write jobs".into())
        })?;
        let conv = conv.as_ref().ok_or_else(|| {
            Error::PathError("internal error: missing conversion config for write jobs".into())
        })?;
        crate::trace::trace_span!("grit.checkout.write");
        execute_write_jobs(
            repo,
            rules,
            &config,
            conv,
            work_tree,
            &mut dir_cache,
            &mut delayed,
            &write_jobs,
            &parallel_indices,
            apply.parallel_prepare_blobs,
            apply.checkout_parallel_threshold,
            apply.skip_parallel_write_ramp,
            &mut progress,
            cancel,
        )?
    };

    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(plan.len());
    let mut write_iter = write_outcomes.into_iter();
    for slot in plan {
        match slot {
            CheckoutPlanSlot::Ready(entry) => new_entries.push(entry),
            CheckoutPlanSlot::Write => {
                match write_iter.next().ok_or_else(|| {
                    Error::PathError("internal error: missing parallel checkout outcome".into())
                })? {
                    WriteJobOutcome::Index(entry) => {
                        new_entries.push(entry);
                    }
                    WriteJobOutcome::Delayed(target) => {
                        pending_delayed.push(target);
                    }
                }
            }
        }
    }

    if !pending_delayed.is_empty() {
        let rules = rules.as_ref().ok_or_else(|| {
            Error::PathError("internal error: missing checkout rules for delayed writes".into())
        })?;
        finish_delayed_checkouts(
            repo,
            rules,
            work_tree,
            &mut delayed,
            &pending_delayed,
            &mut dir_cache,
            &mut new_entries,
        )?;
    }

    remove_empty_parent_dirs_batch(work_tree, &deleted_abs_paths);
    drop(rules);
    let bulk_index_rewrite = changes.len() >= BULK_INDEX_REWRITE_MIN;
    checkout_commit_index_update(
        repo,
        index_mut,
        &wholesale_dirs_removed,
        &paths_to_remove,
        new_entries,
        &apply.skip_worktree_rows,
        bulk_index_rewrite,
        apply.checkout_result_tree,
        apply.defer_index_disk_write,
    )?;
    progress.finish();
    delete_guard.commit_success();
    progress.finish_poll()
}

#[derive(Clone)]
struct DelayedCheckoutTarget {
    path: String,
    mode: u32,
    oid: ObjectId,
    change: DiffEntry,
}

/// Attribute/conversion context for checkout smudge.
fn checkout_worktree_rules(
    repo: &Repository,
    index: &Index,
    attr_changes: &[DiffEntry],
    config: std::sync::Arc<ConfigSet>,
) -> Result<WorktreeRules> {
    if tree_changes_touch_gitattributes(attr_changes) {
        let prospective_index = prospective_index_for_checkout(index, attr_changes);
        WorktreeRules::from_checkout_prospective_index(repo, &prospective_index, config)
    } else {
        WorktreeRules::from_parts_index_attributes_only(repo, index, config)
    }
}

pub(crate) fn tree_changes_touch_gitattributes(changes: &[DiffEntry]) -> bool {
    use std::ffi::OsStr;
    use std::path::Path;
    changes.iter().any(|change| {
        [change.old_path.as_deref(), change.new_path.as_deref()]
            .into_iter()
            .flatten()
            .any(|path| Path::new(path).file_name() == Some(OsStr::new(".gitattributes")))
    })
}

/// Index after applying `changes`, used to resolve destination `.gitattributes` before writes land.
fn prospective_index_for_checkout(index: &Index, changes: &[DiffEntry]) -> Index {
    let mut idx = index.clone();
    for change in changes {
        if change.status == DiffStatus::Deleted {
            if let Some(path) = &change.old_path {
                idx.remove(path.as_bytes());
            }
            continue;
        }
        let Some(path) = &change.new_path else {
            continue;
        };
        if let Some(old) = &change.old_path {
            if old != path {
                idx.remove(old.as_bytes());
            }
        }
        let mode = parse_git_mode(&change.new_mode);
        idx.add_or_replace(prospective_index_entry(path, change.new_oid, mode));
    }
    idx.sort();
    idx
}

fn prospective_index_entry(path: &str, oid: ObjectId, mode: u32) -> IndexEntry {
    IndexEntry {
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
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.as_bytes().to_vec(),
        base_index_pos: 0,
    }
}

/// Index blob bytes after checkout smudge (EOL, encoding, ident, filters).
///
/// Symlink targets are returned unchanged. When a process filter returns `status=delayed`,
/// returns `Ok(None)` and records the path in `delayed_checkout`; the checkout driver completes
/// those paths via [`DelayedProcessCheckout::finish`].
/// Smudge index-format blob bytes for a worktree path (EOL, filters, ident).
///
/// Returns symlink targets unchanged. Used when writing merge/stash conflict markers
/// and other blobs that are stored LF-normalized in the object database.
pub(crate) fn smudge_bytes_for_worktree_path(
    repo: &Repository,
    rules: &WorktreeRules,
    rel_path: &str,
    mode: u32,
    blob: &[u8],
    oid: &ObjectId,
) -> Result<Vec<u8>> {
    if mode == MODE_SYMLINK {
        return Ok(blob.to_vec());
    }
    match worktree_bytes_from_index_blob(repo, rules, rel_path, mode, blob, oid, None, None)? {
        Some(bytes) => Ok(bytes),
        None => Err(Error::PathError(format!(
            "delayed filter checkout is not supported for '{rel_path}'"
        ))),
    }
}

/// Write a blob from the object database to the worktree with checkout smudge rules.
pub(crate) fn write_smudged_blob_to_worktree(
    repo: &Repository,
    work_tree: &Path,
    rules: &WorktreeRules,
    rel_path: &str,
    oid: &ObjectId,
    mode: u32,
) -> Result<()> {
    let written = write_blob_to_worktree(
        repo,
        rules,
        work_tree,
        rel_path,
        oid,
        mode,
        &mut LeadingDirCache::new(),
    )?;
    if written.is_none() {
        return Err(Error::PathError(format!(
            "delayed filter checkout is not supported for '{rel_path}'"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn worktree_bytes_from_index_blob(
    repo: &Repository,
    rules: &WorktreeRules,
    rel_path: &str,
    mode: u32,
    blob: &[u8],
    oid: &ObjectId,
    smudge_meta: Option<&crate::filter_process::FilterSmudgeMeta>,
    delayed_checkout: Option<&mut DelayedProcessCheckout>,
) -> Result<Option<Vec<u8>>> {
    if mode == MODE_SYMLINK {
        return Ok(Some(blob.to_vec()));
    }
    let oid_hex = oid.to_string();
    let smudge_meta_owned;
    let smudge_meta = if let Some(meta) = smudge_meta {
        meta
    } else {
        smudge_meta_owned = crate::filter_process::smudge_meta_for_checkout(repo, &oid_hex);
        &smudge_meta_owned
    };
    let conv = rules.conversion();
    let file_attrs = rules.file_attrs(rel_path, false);
    crlf::convert_to_worktree(
        blob,
        rel_path,
        conv,
        &file_attrs,
        Some(&oid_hex),
        Some(smudge_meta),
        delayed_checkout,
        Some(rules.filter_process()),
    )
    .map_err(Error::from)
}

fn finish_bulk_write_jobs(
    repo: &Repository,
    rules: &WorktreeRules,
    work_tree: &Path,
    dir_cache: &mut LeadingDirCache,
    delayed: &mut DelayedProcessCheckout,
    write_outcomes: Vec<WriteJobOutcome>,
) -> Result<Vec<IndexEntry>> {
    let mut new_entries = Vec::with_capacity(write_outcomes.len());
    let mut pending_delayed = Vec::new();
    for outcome in write_outcomes {
        match outcome {
            WriteJobOutcome::Index(entry) => {
                new_entries.push(entry);
            }
            WriteJobOutcome::Delayed(target) => {
                pending_delayed.push(target);
            }
        }
    }
    finish_delayed_checkouts(
        repo,
        rules,
        work_tree,
        delayed,
        &pending_delayed,
        dir_cache,
        &mut new_entries,
    )?;
    Ok(new_entries)
}

#[allow(clippy::too_many_arguments)]
fn finish_delayed_checkouts(
    repo: &Repository,
    rules: &WorktreeRules,
    work_tree: &Path,
    delayed: &mut DelayedProcessCheckout,
    pending: &[DelayedCheckoutTarget],
    dir_cache: &mut LeadingDirCache,
    new_entries: &mut Vec<IndexEntry>,
) -> Result<()> {
    if delayed.entries.is_empty() {
        return Ok(());
    }
    let finish_result = delayed.finish(
        rules.filter_process(),
        |path, meta| smudge_retry_after_delay(repo, rules, path, meta),
        |path, data| {
            let target = pending.iter().find(|t| t.path == path).ok_or_else(|| {
                format!("internal error: delayed checkout for unknown path '{path}'")
            })?;
            let entry = write_checkout_entry(
                work_tree,
                path,
                WorktreeFileContent::Bytes(data),
                target.mode,
                &target.change,
                dir_cache,
                target.oid,
            )
            .map_err(|e| e.to_string())?;
            new_entries.push(entry);
            Ok(())
        },
    );
    match finish_result {
        Ok(()) => Ok(()),
        Err(DelayedCheckoutError::Reported(problems)) => {
            let msg = problems
                .iter()
                .map(|p| match p {
                    crate::filter_process::DelayedCheckoutProblem::FilterSignaledAvailable {
                        filter_cmd,
                        path,
                    } => format!(
                        "external filter '{filter_cmd}' signaled that '{path}' is now available although it has not been delayed earlier"
                    ),
                    crate::filter_process::DelayedCheckoutProblem::PathNotFilteredProperly {
                        path,
                    } => format!("'{path}' was not filtered properly"),
                })
                .collect::<Vec<_>>()
                .join("; ");
            Err(Error::PathError(msg))
        }
        Err(DelayedCheckoutError::Transport(msg)) => Err(Error::PathError(msg)),
    }
}

fn smudge_retry_after_delay(
    repo: &Repository,
    rules: &WorktreeRules,
    rel_path: &str,
    meta: &FilterSmudgeMeta,
) -> std::result::Result<Vec<u8>, String> {
    let blob_hex = meta
        .blob_hex
        .as_deref()
        .ok_or_else(|| format!("missing blob id for delayed checkout of '{rel_path}'"))?;
    let oid = ObjectId::from_hex(blob_hex)
        .map_err(|e| format!("invalid blob id for delayed checkout of '{rel_path}': {e}"))?;
    let obj = repo
        .odb()
        .read(&oid)
        .map_err(|e| format!("reading blob for delayed checkout of '{rel_path}': {e}"))?;
    // Git `CE_RETRY`: run smudge again without allowing delay (filter serves cached output).
    crlf::convert_to_worktree_eager(
        &obj.data,
        rel_path,
        rules.conversion(),
        &rules.file_attrs(rel_path, false),
        Some(blob_hex),
        Some(meta),
        Some(rules.filter_process()),
    )
    .map_err(|e| e.to_string())
}

/// Build a modified diff for rewriting an unchanged blob, using on-disk mode as `old_mode`.
fn diff_entry_for_worktree_blob_refresh(
    work_tree: &Path,
    rel_path: &str,
    oid: ObjectId,
    target_mode: u32,
) -> DiffEntry {
    let abs = work_tree.join(rel_path);
    let old_mode = fs::symlink_metadata(&abs)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| crate::diff::mode_from_metadata(&meta))
        .unwrap_or(target_mode);
    DiffEntry {
        status: DiffStatus::Modified,
        old_path: Some(rel_path.to_owned()),
        new_path: Some(rel_path.to_owned()),
        old_mode: format_mode(old_mode),
        new_mode: format_mode(target_mode),
        old_oid: oid,
        new_oid: oid,
        score: None,
    }
}

fn parse_git_mode(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or(MODE_REGULAR)
}

fn validate_diff_changes_for_worktree(
    changes: &[DiffEntry],
    protect: ProtectOptions,
) -> Result<()> {
    for change in changes {
        let Some(path) = &change.new_path else {
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        validate_worktree_path(path.as_bytes(), mode, protect)?;
    }
    Ok(())
}

/// True when `path` is a gitlink directory with an initialized submodule (`.git` file or dir).
fn gitlink_worktree_is_populated(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let git_meta = path.join(".git");
    git_meta.is_file() || git_meta.is_dir()
}

/// Drop wholesale-removal candidates when a deleted gitlink under `dir` still has a worktree.
/// Index-derived wholesale directory candidates when switch prepare already trusts removal.
pub(crate) fn trusted_wholesale_directories_for_switch(
    work_tree: &Path,
    index: &Index,
    delete_paths: &[String],
    gitlink_deletes: &HashSet<String>,
) -> BTreeSet<String> {
    let candidates = wholesale_deleted_directories(index, delete_paths);
    filter_wholesale_dirs_with_populated_gitlinks(work_tree, candidates, gitlink_deletes)
}

fn filter_wholesale_dirs_with_populated_gitlinks(
    work_tree: &Path,
    dir_candidates: BTreeSet<String>,
    gitlink_deletes: &HashSet<String>,
) -> BTreeSet<String> {
    if dir_candidates.is_empty() || gitlink_deletes.is_empty() {
        return dir_candidates;
    }
    dir_candidates
        .into_iter()
        .filter(|dir| {
            let prefix = format!("{dir}/");
            !gitlink_deletes.iter().any(|path| {
                (path == dir || path.starts_with(&prefix))
                    && gitlink_worktree_is_populated(&work_tree.join(path))
            })
        })
        .collect()
}

/// True when `path` is an uninitialized submodule checkout (directory, no nested `.git`).
fn gitlink_worktree_matches_index(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    !gitlink_worktree_is_populated(path)
}

/// Remove a gitlink path from the worktree unless it looks like an initialized submodule.
fn remove_uninitialized_gitlink_worktree(abs: &Path) {
    if abs.is_dir() {
        let git_meta = abs.join(".git");
        if git_meta.is_file() || git_meta.is_dir() {
            return;
        }
        let _ = fs::remove_dir_all(abs);
    } else {
        let _ = fs::remove_file(abs);
    }
}

/// Create an empty directory for an uninitialized gitlink, replacing conflicting paths.
fn materialize_gitlink_worktree(
    work_tree: &Path,
    rel_path: &str,
    dir_cache: &mut LeadingDirCache,
) -> Result<()> {
    dir_cache.ensure_parents(work_tree, rel_path)?;
    let file_path = crate::repo_relpath::worktree_abs(work_tree, rel_path);
    if let Ok(meta) = fs::symlink_metadata(&file_path) {
        if meta.file_type().is_symlink() || meta.is_file() {
            fs::remove_file(&file_path)?;
        } else if meta.is_dir() {
            let git_meta = file_path.join(".git");
            if !(git_meta.is_file() || git_meta.is_dir()) {
                fs::remove_dir_all(&file_path)?;
            }
        }
    }
    fs::create_dir_all(&file_path)
        .map_err(|e| Error::PathError(format!("creating gitlink directory '{rel_path}': {e}")))?;
    Ok(())
}

fn is_regular_git_mode(mode: u32) -> bool {
    mode == MODE_REGULAR || mode == MODE_EXECUTABLE
}

fn can_overwrite_regular_in_place(change: &DiffEntry) -> bool {
    change.status == DiffStatus::Modified
        && is_regular_git_mode(parse_git_mode(&change.old_mode))
        && is_regular_git_mode(parse_git_mode(&change.new_mode))
}

/// Blobs larger than this are streamed from the object database into the work tree when
/// checkout copies them verbatim, so checkout memory stays flat however large a file is.
const STREAMED_CHECKOUT_LIMIT: u64 = 1024 * 1024;

/// Buffer size for copying a streamed blob into a work-tree file.
const STREAMED_CHECKOUT_COPY_BUF: usize = 256 * 1024;

/// Contents of one work-tree file being checked out.
pub(crate) enum WorktreeFileContent<'a> {
    /// Bytes already in memory, after any checkout conversion.
    Bytes(&'a [u8]),
    /// A blob payload copied verbatim from an object-database stream (regular files only).
    ///
    /// The stream is spooled to a temporary file next to the destination, which is renamed
    /// into place only once the reader has reached its validated end: a corrupt or truncated
    /// object fails the checkout without leaving a partial file or disturbing the old one.
    Stream(&'a mut dyn Read),
}

/// Copy `reader` into a new temporary file in `dir`, created with the checkout mode for `mode`.
///
/// The temporary file is removed again when reading or writing fails, including when the
/// reader reports a corrupt or truncated object at its end.
fn spool_stream_to_temp(
    dir: &Path,
    rel_path: &str,
    reader: &mut dyn Read,
    mode: u32,
) -> Result<tempfile::NamedTempFile> {
    let write_error = |e: std::io::Error| {
        crate::worktree::map_worktree_io(rel_path, WorktreePathOperation::Replace, e)
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".grit-checkout-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(git_create_mode_for_index_mode(
            mode,
        )));
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut tmp = builder.tempfile_in(dir).map_err(write_error)?;
    let mut buf = vec![0u8; STREAMED_CHECKOUT_COPY_BUF];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(tmp),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        };
        tmp.as_file_mut()
            .write_all(&buf[..n])
            .map_err(write_error)?;
    }
}

/// Read blob `oid` for checkout, or return `None` when it should be streamed instead.
///
/// A regular file whose checkout conversion is the identity
/// ([`crlf::worktree_conversion_is_identity`]) and whose blob is larger than
/// [`STREAMED_CHECKOUT_LIMIT`] is left for [`stream_checkout_entry`]; everything else is read
/// whole. Objects within the limit are read in that one lookup; a streamed object is opened
/// again by [`stream_checkout_entry`]. Bulk callers enable `cache_small` to reuse
/// small loose objects between planning and materialization; direct writes bypass it.
pub(crate) fn read_blob_unless_streamed(
    repo: &Repository,
    conv: &crlf::ConversionConfig,
    attrs: &crlf::FileAttrs,
    mode: u32,
    oid: &ObjectId,
    cache_small: bool,
) -> Result<Option<Object>> {
    if !is_regular_git_mode(mode) || !crlf::worktree_conversion_is_identity(conv, attrs) {
        return repo.odb().read(oid).map(Some);
    }
    match repo
        .odb()
        .read_unless_larger(oid, STREAMED_CHECKOUT_LIMIT, cache_small)?
    {
        BoundedRead::Object(object) => Ok(Some(object)),
        BoundedRead::Larger => Ok(None),
    }
}

/// Check out blob `oid` at `rel_path` by streaming it straight into the work tree.
///
/// For blobs [`read_blob_unless_streamed`] left unread; the content is written verbatim.
///
/// # Errors
///
/// [`Error::ObjectNotFound`] when the object is missing, [`Error::Io`] when its stream is
/// corrupt, and the usual work-tree write errors.
pub(crate) fn stream_checkout_entry(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    mode: u32,
    change: &DiffEntry,
    dir_cache: &mut LeadingDirCache,
    oid: ObjectId,
) -> Result<IndexEntry> {
    repo.odb()
        .with_stream(&oid, |stream| {
            let mut reader = stream.reader;
            write_checkout_entry(
                work_tree,
                rel_path,
                WorktreeFileContent::Stream(&mut *reader),
                mode,
                change,
                dir_cache,
                oid,
            )
        })?
        .ok_or_else(|| Error::ObjectNotFound(oid.to_hex()))
}

/// Write blob `oid` to `rel_path` with checkout conversion, replacing whatever is there.
///
/// Large blobs copied verbatim are streamed from the object database (see
/// [`read_blob_unless_streamed`]); everything else is read, converted and written whole.
/// Returns the written path's metadata, or `None` when a process filter delayed the path.
pub(crate) fn write_blob_to_worktree(
    repo: &Repository,
    rules: &WorktreeRules,
    work_tree: &Path,
    rel_path: &str,
    oid: &ObjectId,
    mode: u32,
    dir_cache: &mut LeadingDirCache,
) -> Result<Option<fs::Metadata>> {
    let attrs = rules.file_attrs(rel_path, false);
    let Some(blob) = read_blob_unless_streamed(repo, rules.conversion(), &attrs, mode, oid, false)?
    else {
        return repo
            .odb()
            .with_stream(oid, |stream| {
                let mut reader = stream.reader;
                write_worktree_content(
                    work_tree,
                    rel_path,
                    WorktreeFileContent::Stream(&mut *reader),
                    mode,
                    dir_cache,
                )
            })?
            .ok_or_else(|| Error::ObjectNotFound(oid.to_hex()))
            .map(Some);
    };
    let Some(bytes) =
        worktree_bytes_from_index_blob(repo, rules, rel_path, mode, &blob.data, oid, None, None)?
    else {
        return Ok(None);
    };
    write_to_worktree_cached(work_tree, rel_path, &bytes, mode, dir_cache).map(Some)
}

fn write_checkout_entry(
    work_tree: &Path,
    rel_path: &str,
    content: WorktreeFileContent<'_>,
    mode: u32,
    change: &DiffEntry,
    dir_cache: &mut LeadingDirCache,
    oid: ObjectId,
) -> Result<IndexEntry> {
    let index_path = crate::repo_relpath::storage_bytes(rel_path).into_owned();
    let in_place_bytes = match content {
        WorktreeFileContent::Bytes(data)
            if mode != MODE_SYMLINK && can_overwrite_regular_in_place(change) =>
        {
            Some(data)
        }
        _ => None,
    };
    if let Some(data) = in_place_bytes {
        dir_cache.ensure_parents(work_tree, rel_path)?;
        let abs_path = crate::repo_relpath::worktree_abs(work_tree, rel_path);
        if let Ok(mut file) = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&abs_path)
        {
            file.write_all(data).map_err(|e| {
                Error::PathError(format!("writing '{rel_path}' during checkout: {e}"))
            })?;
            drop(file);
            let old_mode = parse_git_mode(&change.old_mode);
            apply_in_place_git_mode_transition_at_path(&abs_path, old_mode, mode)?;
            let meta = written_file_symlink_metadata(&abs_path)?;
            return Ok(entry_from_metadata(&meta, &index_path, oid, mode));
        }
    }

    let meta = write_worktree_content(work_tree, rel_path, content, mode, dir_cache)?;
    Ok(entry_from_metadata(&meta, &index_path, oid, mode))
}

/// Adjust only the executable bit for an in-place overwrite, preserving umask-driven permissions.
#[cfg(unix)]
fn apply_in_place_git_mode_transition_at_path(
    abs_path: &Path,
    old_mode: u32,
    new_mode: u32,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let old_exec = old_mode == MODE_EXECUTABLE;
    let new_exec = new_mode == MODE_EXECUTABLE;
    if old_exec == new_exec {
        return Ok(());
    }
    let mut perms = fs::metadata(abs_path)?.permissions();
    let mut bits = perms.mode();
    const ALL_EXEC: u32 = 0o111;
    if new_exec {
        // Match Git: grant execute wherever read is already present (0644→0755 under umask 022,
        // 0600→0700 under umask 077).
        bits = grant_execute_from_read_bits(bits);
    } else {
        bits &= !ALL_EXEC;
    }
    perms.set_mode(bits);
    fs::set_permissions(abs_path, perms).map_err(Error::Io)
}

#[cfg(not(unix))]
fn apply_in_place_git_mode_transition_at_path(
    _abs_path: &Path,
    _old_mode: u32,
    _new_mode: u32,
) -> Result<()> {
    Ok(())
}

/// Tracks leading directories already verified or created during one checkout pass.
pub(crate) struct LeadingDirCache {
    ready: HashSet<PathBuf>,
    protect: ProtectOptions,
}

fn precreate_checkout_parent_dirs(
    work_tree: &Path,
    jobs: &[CheckoutWriteJob<'_>],
    cache: &mut LeadingDirCache,
) {
    let mut dirs = BTreeSet::new();
    for job in jobs {
        let Some(parent) = Path::new(&job.path).parent() else {
            continue;
        };
        if parent.as_os_str().is_empty() {
            continue;
        }
        dirs.insert(work_tree.join(parent));
    }
    for dir in dirs {
        if cache.ready.contains(&dir) {
            continue;
        }
        let _ = fs::create_dir_all(&dir);
        cache.ready.insert(dir);
    }
}

impl LeadingDirCache {
    pub(crate) fn new() -> Self {
        Self::with_protect(ProtectOptions::default())
    }

    pub(crate) fn with_protect(protect: ProtectOptions) -> Self {
        Self {
            ready: HashSet::new(),
            protect,
        }
    }

    /// Share parent-directory state from planning so workers do not re-stat prepared paths.
    pub(crate) fn with_ready_from(source: &Self) -> Self {
        Self {
            ready: source.ready.clone(),
            protect: source.protect,
        }
    }

    fn ensure_parents(&mut self, work_tree: &Path, rel_path: &str) -> Result<()> {
        let rel_bytes = crate::repo_relpath::storage_bytes(rel_path);
        let parent = crate::repo_relpath::parent_dir(&rel_bytes);
        if parent.is_empty() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let mut cur = work_tree.to_path_buf();
            for comp in parent.split(|&b| b == b'/') {
                if comp.is_empty() {
                    continue;
                }
                cur.push(OsStr::from_bytes(comp));
                if self.ready.contains(&cur) {
                    continue;
                }
                if let Ok(meta) = leading_dir_metadata(&cur) {
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        fs::remove_file(&cur)?;
                    }
                }
                if fs::create_dir(&cur).is_err() {
                    // Exists as a directory already.
                }
                self.ready.insert(cur.clone());
            }
        }
        #[cfg(not(unix))]
        {
            use std::path::Component;
            let parent_str = std::str::from_utf8(&parent)
                .map_err(|_| Error::Message("non-UTF-8 parent path during checkout".into()))?;
            let path = Path::new(parent_str);
            let mut cur = work_tree.to_path_buf();
            for comp in path.components() {
                if let Component::Normal(name) = comp {
                    cur.push(name);
                    if self.ready.contains(&cur) {
                        continue;
                    }
                    if let Ok(meta) = leading_dir_metadata(&cur) {
                        if meta.file_type().is_symlink() || !meta.is_dir() {
                            fs::remove_file(&cur)?;
                        }
                    }
                    if fs::create_dir(&cur).is_err() {
                        // Exists as a directory already.
                    }
                    self.ready.insert(cur.clone());
                }
            }
        }
        Ok(())
    }
}

fn leading_dir_metadata(path: &Path) -> Result<fs::Metadata> {
    #[cfg(test)]
    syscall_probe::note_metadata(path, syscall_probe::MetadataKind::Directory);
    fs::symlink_metadata(path).map_err(Error::Io)
}

fn written_file_symlink_metadata(path: &Path) -> Result<fs::Metadata> {
    #[cfg(test)]
    syscall_probe::note_metadata(path, syscall_probe::MetadataKind::WrittenFile);
    fs::symlink_metadata(path).map_err(Error::Io)
}

/// Stat a path just written through `file`, after close (no durability fsync).
///
/// Status and refresh paths probe the worktree with `symlink_metadata` on the path. On Windows,
/// `File::metadata()` on an open handle can disagree with that probe (Git `mingw_stat`), which
/// forces a full re-hash on the first status after clone checkout. Closing the handle is enough
/// for path stat to match status; per-file `sync_all` is not required and regresses clone checkout.
fn written_file_metadata(file: std::fs::File, path: &Path) -> Result<fs::Metadata> {
    drop(file);
    written_file_symlink_metadata(path)
}

/// Grant execute bits wherever read is already present (Git index executable semantics).
#[cfg(unix)]
fn grant_execute_from_read_bits(bits: u32) -> u32 {
    bits | ((bits & 0o444) >> 2)
}

/// Mode passed to `open(2)` when creating a fresh worktree file (before the process umask).
#[cfg(unix)]
fn git_create_mode_for_index_mode(mode: u32) -> u32 {
    if mode == MODE_EXECUTABLE {
        0o777
    } else {
        0o666
    }
}

/// Create a new regular file and write `data`, letting the kernel apply umask to the create mode.
fn write_fresh_worktree_blob(
    abs_path: &Path,
    rel_path: &str,
    data: &[u8],
    mode: u32,
) -> Result<fs::Metadata> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(git_create_mode_for_index_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(abs_path).map_err(|e| {
        crate::worktree::map_worktree_io(rel_path, WorktreePathOperation::Replace, e)
    })?;
    io_with_path(
        rel_path,
        WorktreePathOperation::Replace,
        file.write_all(data),
    )?;
    written_file_metadata(file, abs_path)
}

/// Adjust an existing file to an executable index mode (in-place checkout only).
///
/// Fresh files are created with mode `0777` so umask can mask
/// execute independently of read; this helper propagates execute from read when the prior mode
/// is already fixed on disk.
pub fn apply_index_file_mode(abs_path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if mode != MODE_EXECUTABLE {
            return Ok(());
        }
        let mut perms = fs::metadata(abs_path)?.permissions();
        let bits = grant_execute_from_read_bits(perms.mode());
        perms.set_mode(bits);
        fs::set_permissions(abs_path, perms)?;
    }
    // Windows has no POSIX mode bits; the executable bit is not represented in
    // the filesystem, so this is a no-op there.
    #[cfg(not(unix))]
    let _ = (abs_path, mode);
    Ok(())
}

/// Write data to a working tree file, handling symlinks and executable bits.
pub(crate) fn write_to_worktree_cached(
    work_tree: &Path,
    rel_path: &str,
    data: &[u8],
    mode: u32,
    dir_cache: &mut LeadingDirCache,
) -> Result<fs::Metadata> {
    write_worktree_content(
        work_tree,
        rel_path,
        WorktreeFileContent::Bytes(data),
        mode,
        dir_cache,
    )
}

/// Write `content` to a fresh working tree file, replacing whatever is at the path.
fn write_worktree_content(
    work_tree: &Path,
    rel_path: &str,
    content: WorktreeFileContent<'_>,
    mode: u32,
    dir_cache: &mut LeadingDirCache,
) -> Result<fs::Metadata> {
    let rel_bytes = crate::repo_relpath::storage_bytes(rel_path);
    validate_worktree_path(&rel_bytes, mode, dir_cache.protect)?;
    let abs_path = crate::index::worktree_path_from_index_rel(work_tree, &rel_bytes);

    dir_cache.ensure_parents(work_tree, rel_path)?;
    if let Some(parent) = abs_path.parent() {
        if !dir_cache.ready.contains(parent) {
            io_with_path(
                rel_path,
                WorktreePathOperation::Create,
                fs::create_dir_all(parent),
            )?;
            dir_cache.ready.insert(parent.to_path_buf());
        }
    }

    enum Ready<'a> {
        Bytes(&'a [u8]),
        Spooled(tempfile::NamedTempFile),
    }
    let ready = match content {
        WorktreeFileContent::Bytes(data) => Ready::Bytes(data),
        WorktreeFileContent::Stream(_) if mode == MODE_SYMLINK => {
            return Err(Error::PathError(format!(
                "symlink target for '{rel_path}' must be in memory"
            )));
        }
        WorktreeFileContent::Stream(reader) => {
            let dir = abs_path.parent().unwrap_or(work_tree);
            Ready::Spooled(spool_stream_to_temp(dir, rel_path, reader, mode)?)
        }
    };

    // Remove existing file/dir/symlink at target path. Use symlink_metadata + is_symlink so we
    // replace symlinked paths (e.g. `D` → `untracked`) before creating a real directory tree.
    if let Ok(meta) = fs::symlink_metadata(&abs_path) {
        if meta.file_type().is_symlink() {
            io_with_path(
                rel_path,
                WorktreePathOperation::Remove,
                fs::remove_file(&abs_path),
            )?;
        } else if meta.is_dir() {
            io_with_path(
                rel_path,
                WorktreePathOperation::Remove,
                fs::remove_dir_all(&abs_path),
            )?;
        } else {
            io_with_path(
                rel_path,
                WorktreePathOperation::Remove,
                fs::remove_file(&abs_path),
            )?;
        }
    }

    let data = match ready {
        Ready::Spooled(tmp) => {
            let file = tmp.persist(&abs_path).map_err(|e| {
                crate::worktree::map_worktree_io(rel_path, WorktreePathOperation::Replace, e.error)
            })?;
            return written_file_metadata(file, &abs_path);
        }
        Ready::Bytes(data) => data,
    };
    if mode == MODE_SYMLINK {
        let target = std::str::from_utf8(data).map_err(|_| {
            Error::PathError(format!("symlink target for '{rel_path}' is not UTF-8"))
        })?;
        #[cfg(unix)]
        io_with_path(
            rel_path,
            WorktreePathOperation::Create,
            std::os::unix::fs::symlink(target, &abs_path),
        )?;
        // Windows lacks unprivileged symlink creation; materialise the link as a
        // regular file containing the target path so the worktree stays populated.
        #[cfg(not(unix))]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&abs_path)
                .map_err(|e| {
                    crate::worktree::map_worktree_io(rel_path, WorktreePathOperation::Replace, e)
                })?;
            io_with_path(
                rel_path,
                WorktreePathOperation::Replace,
                file.write_all(target.as_bytes()),
            )?;
            return written_file_metadata(file, &abs_path);
        }
        #[cfg(unix)]
        return written_file_symlink_metadata(&abs_path);
    }
    write_fresh_worktree_blob(&abs_path, rel_path, data, mode)
}

/// Remove empty parent directories up to (but not including) `work_tree`.
pub fn remove_empty_parent_dirs(work_tree: &Path, path: &Path) {
    remove_empty_parent_dirs_batch(work_tree, &[path.to_path_buf()]);
}

fn remove_empty_parent_dirs_batch(work_tree: &Path, deleted_paths: &[PathBuf]) {
    let cwd = Some(crate::environment::Environment::empty().cwd.clone());
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for path in deleted_paths {
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == work_tree {
                break;
            }
            dirs.insert(dir.to_path_buf());
            current = dir.parent();
        }
    }
    let mut ordered: Vec<PathBuf> = dirs.into_iter().collect();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in ordered {
        if cwd
            .as_ref()
            .is_some_and(|cwd| cwd == &dir || cwd.starts_with(&dir))
        {
            continue;
        }
        let _ = fs::remove_dir(&dir);
    }
}

#[cfg(test)]
mod syscall_probe {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum MetadataKind {
        Directory,
        WrittenFile,
    }

    thread_local! {
        static COUNTS: RefCell<HashMap<(PathBuf, MetadataKind), u32>> =
            RefCell::new(HashMap::new());
    }

    pub fn reset() {
        COUNTS.with(|c| c.borrow_mut().clear());
    }

    pub fn note_metadata(path: &Path, kind: MetadataKind) {
        COUNTS.with(|c| {
            let mut map = c.borrow_mut();
            *map.entry((path.to_path_buf(), kind)).or_insert(0) += 1;
        });
    }

    pub fn max_per_directory() -> u32 {
        COUNTS.with(|c| {
            c.borrow()
                .iter()
                .filter(|((_, k), _)| *k == MetadataKind::Directory)
                .map(|(_, v)| *v)
                .max()
                .unwrap_or(0)
        })
    }

    pub fn max_per_written_file() -> u32 {
        COUNTS.with(|c| {
            c.borrow()
                .iter()
                .filter(|((_, k), _)| *k == MetadataKind::WrittenFile)
                .map(|(_, v)| *v)
                .max()
                .unwrap_or(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::entry_from_stat;
    use crate::objects::ObjectKind;
    use crate::progress::{ProgressPhase, ProgressSink};
    use crate::repo::init_repository;
    use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
    use tempfile::TempDir;

    #[derive(Default)]
    struct CheckoutProgressRecorder {
        started: Vec<(ProgressPhase, Option<u64>)>,
        file_units: u64,
        finished: u32,
    }

    impl ProgressSink for CheckoutProgressRecorder {
        fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
            self.started.push((phase, total));
        }
        fn inc(&mut self, units: u64) {
            self.file_units += units;
        }
        fn finish(&mut self) {
            self.finished += 1;
        }
    }

    fn commit_all(repo: &Repository, message: &str) -> ObjectId {
        use crate::objects::{serialize_commit, CommitData};
        use crate::refs;
        let work_tree = repo.work_tree().expect("work tree");
        let mut index = repo.load_index().expect("index");
        for entry in std::fs::read_dir(work_tree).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let rel = path.strip_prefix(work_tree).expect("rel").to_string_lossy();
            let data = fs::read(&path).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let e = entry_from_stat(&path, rel.as_bytes(), oid, MODE_REGULAR).expect("entry");
            index.add_or_replace(e);
        }
        index.sort();
        let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        repo.write_index(&mut index).expect("write index");
        let parent = refs::resolve_ref(repo.git_dir(), "HEAD").ok();
        let commit_data = CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: "Test <t@example.com>".to_owned(),
            committer: "Test <t@example.com>".to_owned(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("{message}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let bytes = serialize_commit(&commit_data);
        let commit_oid = repo
            .odb()
            .write(ObjectKind::Commit, &bytes)
            .expect("commit");
        refs::write_ref(repo.git_dir(), "HEAD", &commit_oid).expect("head");
        tree
    }

    #[test]
    fn wholesale_deleted_directories_skips_dir_with_remaining_gitlink() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("d005")).expect("mkdir");
        let mut index = repo.load_index().expect("index");
        let mut delete_paths = Vec::new();
        for i in 0..50 {
            let rel = format!("d005/f{i:02}.txt");
            let abs = wt.join(&rel);
            std::fs::write(&abs, format!("v{i}\n")).expect("write");
            let data = fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            index.add_or_replace(
                entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).expect("entry"),
            );
            delete_paths.push(rel);
        }
        let sub_rel = "d005/submodule";
        std::fs::create_dir_all(wt.join(sub_rel)).expect("gitlink dir");
        let sub_oid = repo.odb().write(ObjectKind::Blob, b"sub\n").expect("blob");
        index.add_or_replace(
            entry_from_stat(&wt.join(sub_rel), sub_rel.as_bytes(), sub_oid, MODE_GITLINK)
                .expect("gitlink entry"),
        );
        index.sort();
        repo.write_index(&mut index).expect("write index");
        let wholesale = wholesale_deleted_directories(&index, &delete_paths);
        assert!(
            wholesale.is_empty(),
            "gitlink under dir must block wholesale removal"
        );
    }

    #[test]
    fn bulk_mod_delete_checkout_eligible_accepts_mixed_mod_and_delete() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let config = repo.config().expect("config");
        let index = repo.load_index().expect("index");
        let rules = WorktreeRules::from_parts_index_attributes_only(&repo, &index, config.clone())
            .expect("rules");
        let mut changes = Vec::new();
        for i in 0..40 {
            changes.push(DiffEntry {
                status: DiffStatus::Modified,
                old_path: Some(format!("f{i:03}.txt")),
                new_path: Some(format!("f{i:03}.txt")),
                old_mode: "100644".into(),
                new_mode: "100644".into(),
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                score: None,
            });
        }
        for i in 0..40 {
            changes.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(format!("gone{i:03}.txt")),
                new_path: None,
                old_mode: "100644".into(),
                new_mode: String::new(),
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                score: None,
            });
        }
        assert!(bulk_mod_delete_checkout_eligible(&changes, &rules));
    }

    #[test]
    fn bulk_mod_delete_checkout_eligible_accepts_mixed_add_mod_and_delete() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let config = repo.config().expect("config");
        let index = repo.load_index().expect("index");
        let rules = WorktreeRules::from_parts_index_attributes_only(&repo, &index, config.clone())
            .expect("rules");
        let mut changes = Vec::new();
        for i in 0..20 {
            changes.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(format!("new{i:03}.txt")),
                old_mode: String::new(),
                new_mode: "100644".into(),
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                score: None,
            });
        }
        for i in 0..20 {
            changes.push(DiffEntry {
                status: DiffStatus::Modified,
                old_path: Some(format!("f{i:03}.txt")),
                new_path: Some(format!("f{i:03}.txt")),
                old_mode: "100644".into(),
                new_mode: "100644".into(),
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                score: None,
            });
        }
        for i in 0..20 {
            changes.push(DiffEntry {
                status: DiffStatus::Deleted,
                old_path: Some(format!("gone{i:03}.txt")),
                new_path: None,
                old_mode: "100644".into(),
                new_mode: String::new(),
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                score: None,
            });
        }
        assert!(bulk_mod_delete_checkout_eligible(&changes, &rules));
    }

    #[test]
    fn wholesale_deleted_directories_detects_full_top_level_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("d005")).expect("mkdir");
        let mut index = repo.load_index().expect("index");
        let mut delete_paths = Vec::new();
        for i in 0..50 {
            let rel = format!("d005/f{i:02}.txt");
            let abs = wt.join(&rel);
            std::fs::write(&abs, format!("v{i}\n")).expect("write");
            let data = fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            index.add_or_replace(
                entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).expect("entry"),
            );
            delete_paths.push(rel);
        }
        index.sort();
        repo.write_index(&mut index).expect("write index");
        assert_eq!(delete_paths.len(), 50);
        let wholesale = wholesale_deleted_directories(&index, &delete_paths);
        assert_eq!(wholesale, BTreeSet::from(["d005".to_owned()]));
    }

    #[test]
    fn wholesale_directory_removal_skips_when_untracked_present() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("d")).expect("mkdir");
        let mut index = repo.load_index().expect("index");
        let mut delete_paths = Vec::new();
        for i in 0..8 {
            let rel = format!("d/f{i}.txt");
            let abs = wt.join(&rel);
            std::fs::write(&abs, format!("v{i}\n")).expect("write");
            let data = fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            index.add_or_replace(
                entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).expect("entry"),
            );
            delete_paths.push(rel);
        }
        std::fs::write(wt.join("d/untracked.txt"), b"keep me\n").expect("untracked");
        index.sort();
        let delete_set: HashSet<String> = delete_paths.iter().cloned().collect();
        assert!(!super::wholesale_directory_safe_to_remove(
            wt,
            "d",
            &delete_set,
            false
        ));
        let wholesale = super::wholesale_deleted_directories_on_disk(
            &repo,
            wt,
            &index,
            &delete_paths,
            false,
            &CheckoutApplyOptions::default(),
            &HashSet::new(),
        );
        assert!(wholesale.is_empty());
    }

    #[test]
    fn checkout_deletes_gitlink_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let wt = tmp.path().join("wt");
        fs::create_dir_all(&wt).expect("mkdir");
        let repo = init_repository(&wt, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        let gitlink: ObjectId = "855827c583bc30645ba427885caa40c5b81764d2"
            .parse()
            .expect("oid");
        fs::write(wt.join("a"), b"a\n").expect("a");
        let a_oid = repo.odb().write(ObjectKind::Blob, b"a\n").expect("blob");
        let mut index = repo.load_index().expect("index");
        index.add_or_replace(entry_from_stat(&wt.join("a"), b"a", a_oid, MODE_REGULAR).expect("e"));
        index.add_or_replace(prospective_index_entry("sub", gitlink, MODE_GITLINK));
        index.sort();
        let with_sub =
            write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
                .expect("tree with sub");
        fs::write(wt.join("b"), b"b\n").expect("b");
        let b_oid = repo.odb().write(ObjectKind::Blob, b"b\n").expect("blob");
        index.remove(b"sub");
        index.add_or_replace(entry_from_stat(&wt.join("b"), b"b", b_oid, MODE_REGULAR).expect("e"));
        index.sort();
        let no_sub = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree no sub");

        checkout_between_trees(&repo, None, &with_sub).expect("checkout gitlink");
        assert!(wt.join("sub").is_dir());
        checkout_between_trees(&repo, Some(&with_sub), &no_sub).expect("remove gitlink");
        assert!(
            !wt.join("sub").exists(),
            "gitlink directory should be removed on delete"
        );
    }

    #[test]
    fn populated_checkout_reports_file_count() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        for i in 0..300 {
            fs::write(wt.join(format!("f{i}.txt")), format!("body {i}\n")).expect("write");
        }
        let tree = commit_all(&repo, "many files");
        let mut index = Index::new();
        repo.write_index(&mut index).expect("empty index");

        let mut rec = CheckoutProgressRecorder::default();
        checkout_between_trees_with_progress(&repo, None, &tree, &mut rec, &NeverCancel)
            .expect("checkout");

        let index = repo.load_index().expect("index after");
        assert_eq!(
            rec.started,
            vec![(ProgressPhase::CheckingOutFiles, Some(300))]
        );
        assert_eq!(rec.finished, 1);
        assert_eq!(
            rec.file_units,
            index.entries.len() as u64,
            "progress file count should match index entries (one unit per checked-out path)"
        );
        assert_eq!(index.entries.len(), 300);
    }

    #[test]
    fn slow_checkout_reports_total() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        for d in 0..3 {
            fs::write(wt.join(format!("a{d}.txt")), format!("v0 {d}\n")).expect("write");
        }
        let head = commit_all(&repo, "initial");
        for d in 0..3 {
            fs::write(wt.join(format!("a{d}.txt")), format!("v1 {d}\n")).expect("write");
        }
        let to_tree = commit_all(&repo, "updated");
        let changes = diff_trees(repo.odb(), Some(&head), Some(&to_tree), "").expect("diff");

        let mut rec = CheckoutProgressRecorder::default();
        checkout_between_trees_with_progress(&repo, Some(&head), &to_tree, &mut rec, &NeverCancel)
            .expect("checkout");

        assert_eq!(rec.started.len(), 1);
        assert_eq!(rec.started[0].0, ProgressPhase::CheckingOutFiles);
        assert_eq!(rec.started[0].1, Some(changes.len() as u64));
        assert_eq!(rec.file_units, changes.len() as u64);
        assert_eq!(rec.finished, 1);
    }

    #[test]
    fn checkout_syscall_counts_one_metadata_per_dir_and_file() {
        syscall_probe::reset();
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        for d in 0..5 {
            let dir = wt.join(format!("dir{d}"));
            fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..20 {
                fs::write(dir.join(format!("f{f}.txt")), format!("body {d}-{f}\n")).expect("w");
            }
        }
        let head = commit_all(&repo, "initial");
        for d in 0..5 {
            let dir = wt.join(format!("dir{d}"));
            for f in 0..20 {
                fs::write(dir.join(format!("f{f}.txt")), format!("new {d}-{f}\n")).expect("w");
            }
        }
        let mut index = repo.load_index().expect("index");
        for d in 0..5 {
            for f in 0..20 {
                let rel = format!("dir{d}/f{f}.txt");
                let abs = wt.join(&rel);
                let data = fs::read(&abs).expect("read");
                let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
                let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).expect("e");
                index.add_or_replace(entry);
            }
        }
        index.sort();
        let to_tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("t");

        syscall_probe::reset();
        checkout_between_trees(&repo, Some(&head), &to_tree).expect("checkout");

        assert!(
            syscall_probe::max_per_directory() <= 1,
            "each directory should incur at most one tracked metadata call"
        );
        assert!(
            syscall_probe::max_per_written_file() <= 1,
            "each written file should incur at most one post-write metadata call"
        );
    }

    #[test]
    fn bulk_added_checkout_applies_preverified_wholesale_removals() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("old")).expect("mkdir old");
        std::fs::create_dir_all(wt.join("new")).expect("mkdir new");
        let mut index = repo.load_index().expect("index");
        let mut changes = Vec::new();
        for i in 0..40 {
            let old_rel = format!("old/f{i:02}.txt");
            let new_rel = format!("new/f{i:02}.txt");
            let old_abs = wt.join(&old_rel);
            let new_abs = wt.join(&new_rel);
            std::fs::write(&old_abs, format!("old {i}\n")).expect("write old");
            let old_data = fs::read(&old_abs).expect("read old");
            let old_oid = repo.odb().write(ObjectKind::Blob, &old_data).expect("blob");
            index.add_or_replace(
                entry_from_stat(&old_abs, old_rel.as_bytes(), old_oid, MODE_REGULAR)
                    .expect("old entry"),
            );
            std::fs::write(&new_abs, format!("new {i}\n")).expect("write new");
            let new_data = fs::read(&new_abs).expect("read new");
            let new_oid = repo.odb().write(ObjectKind::Blob, &new_data).expect("blob");
            changes.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(new_rel),
                old_mode: String::new(),
                new_mode: "100644".into(),
                old_oid: ObjectId::zero(),
                new_oid,
                score: None,
            });
        }
        index.sort();
        let mut preverified = BTreeSet::new();
        preverified.insert("old".into());
        checkout_tree_changes_with_index_opts(
            &repo,
            &changes,
            &mut index,
            &mut NullProgress,
            &NeverCancel,
            CheckoutApplyOptions {
                parallel_prepare_blobs: true,
                checkout_parallel_threshold: Some(0),
                trust_wholesale_dir_removal: true,
                preverified_wholesale_dirs: preverified,
                wholesale_safety_precomputed: true,
                skip_bulk_checkout_fast_path: false,
                checkout_result_tree: None,
                defer_index_disk_write: false,
                ..CheckoutApplyOptions::default()
            },
            None,
        )
        .expect("checkout");
        assert!(
            !wt.join("old").exists(),
            "wholesale directory should be removed during bulk-add checkout"
        );
        for i in 0..40 {
            assert!(wt.join(format!("new/f{i:02}.txt")).is_file());
        }
    }

    #[cfg(unix)]
    #[test]
    fn bulk_added_checkout_drops_index_when_wholesale_removal_fails() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("old")).expect("mkdir old");
        std::fs::create_dir_all(wt.join("new")).expect("mkdir new");
        let mut index = repo.load_index().expect("index");
        let mut changes = Vec::new();
        for i in 0..40 {
            let old_rel = format!("old/f{i:02}.txt");
            let new_rel = format!("new/f{i:02}.txt");
            let old_abs = wt.join(&old_rel);
            let new_abs = wt.join(&new_rel);
            std::fs::write(&old_abs, format!("old {i}\n")).expect("write old");
            let old_data = fs::read(&old_abs).expect("read old");
            let old_oid = repo.odb().write(ObjectKind::Blob, &old_data).expect("blob");
            index.add_or_replace(
                entry_from_stat(&old_abs, old_rel.as_bytes(), old_oid, MODE_REGULAR)
                    .expect("old entry"),
            );
            std::fs::write(&new_abs, format!("new {i}\n")).expect("write new");
            let new_data = fs::read(&new_abs).expect("read new");
            let new_oid = repo.odb().write(ObjectKind::Blob, &new_data).expect("blob");
            changes.push(DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(new_rel),
                old_mode: String::new(),
                new_mode: "100644".into(),
                old_oid: ObjectId::zero(),
                new_oid,
                score: None,
            });
        }
        index.sort();
        let mut perms = std::fs::metadata(wt.join("old"))
            .expect("meta")
            .permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(wt.join("old"), perms).expect("chmod old");

        let mut preverified = BTreeSet::new();
        preverified.insert("old".into());
        checkout_tree_changes_with_index_opts(
            &repo,
            &changes,
            &mut index,
            &mut NullProgress,
            &NeverCancel,
            CheckoutApplyOptions {
                parallel_prepare_blobs: true,
                checkout_parallel_threshold: Some(0),
                trust_wholesale_dir_removal: true,
                preverified_wholesale_dirs: preverified,
                wholesale_safety_precomputed: true,
                skip_bulk_checkout_fast_path: false,
                checkout_result_tree: None,
                defer_index_disk_write: false,
                ..CheckoutApplyOptions::default()
            },
            None,
        )
        .expect("checkout");
        assert!(
            index.get(b"old/f00.txt", 0).is_none(),
            "index must drop wholesale-deleted paths even when directory removal fails"
        );
        for i in 0..40 {
            assert!(index
                .get(format!("new/f{i:02}.txt").as_bytes(), 0)
                .is_some());
        }
    }

    #[test]
    fn populated_tree_prefetches_promisor_blobs_once_per_chunk() {
        PREFETCH_PROMISOR_FOR_CHECKOUT_CALLS.with(|c| *c.borrow_mut() = 0);
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        for i in 0..=POPULATED_TREE_CHECKOUT_CHUNK {
            let rel = format!("f{i:05}.txt");
            std::fs::write(wt.join(&rel), format!("body {i}\n")).expect("write");
        }
        let tree = commit_all(&repo, "many");
        let mut index = crate::index::Index::new();
        repo.index_handle().write(&mut index).expect("clear index");
        for i in 0..=POPULATED_TREE_CHECKOUT_CHUNK {
            std::fs::remove_file(wt.join(format!("f{i:05}.txt"))).expect("remove");
        }
        checkout_between_trees(&repo, None, &tree).expect("populate");
        let calls = PREFETCH_PROMISOR_FOR_CHECKOUT_CALLS.with(|c| *c.borrow());
        assert!(
            calls >= 2,
            "expected promisor prefetch at least once per populated-tree chunk, got {calls}"
        );
    }

    #[test]
    fn populated_tree_builds_cache_tree_once_across_chunks() {
        // Two full chunks take the bulk path; the short tail takes the per-change path.
        let count = 2 * POPULATED_TREE_CHECKOUT_CHUNK + 5;
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let wt = repo.work_tree().expect("wt");
        for i in 0..count {
            fs::write(wt.join(format!("f{i:05}.txt")), format!("body {i}\n")).expect("write");
        }
        let tree = commit_all(&repo, "many");
        let mut index = Index::new();
        repo.index_handle().write(&mut index).expect("clear index");
        for i in 0..count {
            fs::remove_file(wt.join(format!("f{i:05}.txt"))).expect("remove");
        }

        let builds_before = repo.odb().hot_path_metrics().cache_tree_builds();
        checkout_between_trees(&repo, None, &tree).expect("populate");
        let builds = repo.odb().hot_path_metrics().cache_tree_builds() - builds_before;

        assert_eq!(
            builds, 1,
            "populated checkout should build the cache tree once, not once per chunk"
        );
        let index = repo.load_index().expect("index after");
        assert_eq!(index.entries.len(), count);
        assert_eq!(index.cache_tree_root, Some(tree));
        assert_eq!(
            index.cache_tree.as_ref().map(|t| t.entry_count),
            Some(i32::try_from(count).expect("count fits"))
        );
        assert!(wt.join(format!("f{:05}.txt", count - 1)).is_file());
    }
}
