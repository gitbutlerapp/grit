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

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::crlf::{self, ConversionConfig};
use crate::diff::{diff_trees, format_mode, DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::filter_process::{DelayedCheckoutError, DelayedProcessCheckout, FilterSmudgeMeta};
use crate::index::{
    entry_from_metadata, entry_from_stat, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK,
    MODE_REGULAR, MODE_SYMLINK,
};
use crate::objects::ObjectId;
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp};
use crate::path_icase::paths_equal;
use crate::path_population::validate_worktree_path;
use crate::path_validate::ProtectOptions;
use crate::progress::{NullProgress, ProgressPhase, ProgressSink};
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;

#[path = "checkout_parallel.rs"]
mod checkout_parallel;
use checkout_parallel::{
    execute_write_jobs, is_eligible_for_parallel_checkout, paths_needing_serial_checkout,
    CheckoutPlanSlot, CheckoutWriteJob, WriteJobOutcome,
};

const PARALLEL_PER_FILE_DELETE_MIN: usize = 32;

/// Populate a work tree and index from `tree_oid` (empty index or wholesale replace).
fn checkout_populated_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    checkout_between_trees_slow(repo, None, Some(tree_oid), progress)
}

fn checkout_between_trees_slow(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: Option<&ObjectId>,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    let changes = diff_trees(repo.odb(), from, to, "")?;
    checkout_tree_changes_with_progress(
        repo,
        &changes,
        progress,
        None,
        CheckoutApplyOptions::default(),
        None,
    )
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
    checkout_between_trees_with_progress(repo, from, to, &mut NullProgress)
}

/// Like [`checkout_between_trees`], reporting checkout progress to `progress`.
///
/// Populated-tree checkouts emit [`ProgressPhase::CheckingOutFiles`] with an
/// unknown total; the slow diff-based path reports a total equal to the number
/// of tree changes.
pub fn checkout_between_trees_with_progress(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: &ObjectId,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    if from.is_none() {
        return checkout_populated_tree(repo, to, progress);
    }
    checkout_between_trees_slow(repo, from, Some(to), progress)
}

/// Replace the index and tracked worktree so they match `target_tree` exactly.
///
/// Used by hard reset. When `head_tree` differs from `target_tree`, only paths that
/// changed between the two trees are written first (same as branch checkout); paths
/// that stayed the same are left on disk when already up to date. When both trees
/// are equal (reset to the current commit), only dirty or mismatched paths are
/// rewritten. Tracked paths absent from `target_tree` are removed and the index is
/// rebuilt without unmerged stages.
///
/// # Errors
///
/// Returns an error when the repository has no work tree or checkout I/O fails.
pub fn checkout_hard_to_tree_from_head_tree(
    repo: &Repository,
    head_tree: &ObjectId,
    target_tree: &ObjectId,
) -> Result<()> {
    use crate::porcelain::merge::tree_to_index_entries;

    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    let target_entries = tree_to_index_entries(repo, target_tree, "")?;
    let target_paths: BTreeSet<Vec<u8>> = target_entries.iter().map(|e| e.path.clone()).collect();

    let index = repo.load_index()?;
    let mut paths_in_index: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in &index.entries {
        paths_in_index.insert(entry.path.clone());
    }
    let mut deleted_abs_paths: Vec<PathBuf> = Vec::new();
    for path in &paths_in_index {
        if target_paths.contains(path) {
            continue;
        }
        let abs = crate::index::worktree_path_from_index_rel(work_tree, path);
        if let Some(entry) = index.get(path, 0) {
            if entry.mode == MODE_GITLINK {
                remove_uninitialized_gitlink_worktree(&abs);
            } else if fs::symlink_metadata(&abs).is_ok() {
                let _ = fs::remove_file(&abs);
            }
            deleted_abs_paths.push(abs);
        } else if fs::symlink_metadata(&abs).is_ok() {
            let _ = fs::remove_file(&abs);
            deleted_abs_paths.push(abs);
        }
    }

    let trees_differ = head_tree != target_tree;
    let changes = if trees_differ {
        diff_trees(repo.odb(), Some(head_tree), Some(target_tree), "")?
    } else {
        Vec::new()
    };
    let mut written_by_tree_diff: BTreeSet<Vec<u8>> = BTreeSet::new();
    if !changes.is_empty() {
        checkout_tree_changes(repo, &changes)?;
        for change in &changes {
            if change.status == DiffStatus::Deleted {
                continue;
            }
            if let Some(path) = &change.new_path {
                written_by_tree_diff.insert(crate::repo_relpath::storage_bytes(path).into_owned());
            }
        }
    }

    let index = repo.load_index()?;
    let config = repo.config()?;
    let rules = WorktreeRules::from_parts(repo, &index, config.clone())?;
    let mut delayed = DelayedProcessCheckout::default();
    let mut pending_delayed: Vec<DelayedCheckoutTarget> = Vec::new();
    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(target_entries.len());
    let protect = ProtectOptions::from_config(config.as_ref());
    let mut dir_cache = LeadingDirCache::with_protect(protect);

    for te in target_entries {
        let abs = crate::index::worktree_path_from_index_rel(work_tree, &te.path);
        let mode = te.mode;
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

        if trees_differ && written_by_tree_diff.contains(&te.path) {
            let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
            new_entries.push(entry_from_metadata(&meta, te.path.as_slice(), te.oid, mode));
            continue;
        }

        let path = crate::repo_relpath::storage_string(&te.path);
        if let Some(existing) = index.get(te.path.as_slice(), 0) {
            if crate::diff::path_checkout_skip_blob_write_when_up_to_date_with_config(
                repo.odb(),
                repo.git_dir(),
                work_tree,
                &index,
                existing,
                &te.oid,
                mode,
                &path,
                Some(rules.filter_process()),
                Some(config.as_ref()),
            )? {
                let abs = abs.clone();
                let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                new_entries.push(entry_from_metadata(&meta, te.path.as_slice(), te.oid, mode));
                continue;
            }
        }

        let change = diff_entry_for_worktree_blob_refresh(work_tree, &path, te.oid, mode);
        let object = repo.odb().read(&te.oid)?;
        let worktree_bytes = worktree_bytes_from_index_blob(
            repo,
            &rules,
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
            &worktree_bytes,
            mode,
            &change,
            &mut dir_cache,
            te.oid,
        )?;
        new_entries.push(entry);
    }

    finish_delayed_checkouts(
        repo,
        &rules,
        work_tree,
        &mut delayed,
        &pending_delayed,
        &mut dir_cache,
        &mut new_entries,
    )?;

    remove_empty_parent_dirs_batch(work_tree, &deleted_abs_paths);

    let mut new_index = Index::new();
    new_index.entries = new_entries;
    new_index.sort();
    repo.write_index(&mut new_index)?;
    Ok(())
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
        None,
        CheckoutApplyOptions::default(),
        None,
    )
}

/// Options for [`checkout_tree_changes_with_index_opts`].
#[derive(Clone, Debug, Default)]
pub(crate) struct CheckoutApplyOptions {
    /// Read blob ODB objects in parallel when preparing write jobs.
    ///
    /// Only set when every OID is known to exist (e.g. tree-to-tree switch); general checkout
    /// must keep serial prepare so missing blobs flush earlier writes before failing.
    pub parallel_prepare_blobs: bool,
    /// Override `checkout.thresholdForParallelism` for this apply (switch uses a low threshold).
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
        CheckoutApplyOptions::default(),
        None,
    )
}

/// Like [`checkout_tree_changes_with_index`], with apply-time tuning for trusted callers.
pub(crate) fn checkout_tree_changes_with_index_opts(
    repo: &Repository,
    changes: &[DiffEntry],
    index: &mut Index,
    apply: CheckoutApplyOptions,
    attribute_changes: Option<&[DiffEntry]>,
) -> Result<()> {
    checkout_tree_changes_with_progress(
        repo,
        changes,
        &mut NullProgress,
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
const BULK_DELETE_CHECKOUT_MIN: usize = 32;

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
            Ok(fs::remove_dir_all(&abs).is_ok().then_some(dir.clone()))
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
            if fs::remove_dir_all(&abs).is_ok() {
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
    new_entries: Vec<IndexEntry>,
    bulk_index_rewrite: bool,
    checkout_result_tree: Option<crate::objects::ObjectId>,
    defer_index_disk_write: bool,
) -> Result<()> {
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
        if index_mut.cache_tree.is_some() {
            index_mut.cache_tree_root = Some(tree);
        }
    }
    index_mut.defer_untracked_cache_invalidation = false;
    if defer_index_disk_write {
        return Ok(());
    }
    index_mut.suppress_pre_write_index_housekeeping = bulk_index_rewrite;
    index_mut.skip_racy_smudge_on_write = true;
    let result = repo.write_index_with_post_index_change(index_mut, true, false);
    index_mut.skip_racy_smudge_on_write = false;
    index_mut.suppress_pre_write_index_housekeeping = false;
    result?;
    let index_path = repo.index_path_for_env()?;
    crate::index::advance_index_file_mtime_past_entries(index_mut, &index_path, index_mtime_hint)?;
    index_mut.source_mtime = crate::index::index_file_mtime(&index_path);
    Ok(())
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
) -> Result<()> {
    let checkout_smudge_ctx = crate::filter_process::checkout_smudge_context(repo);
    let mut dir_cache = LeadingDirCache::with_protect(ProtectOptions::from_config(config.as_ref()));
    let mut write_jobs: Vec<CheckoutWriteJob> = Vec::with_capacity(changes.len());
    for change in changes {
        let Some(path) = change.new_path.as_deref() else {
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        let file_attrs = rules.file_attrs(path, false);
        write_jobs.push(CheckoutWriteJob {
            path: path.to_owned(),
            mode,
            oid: change.new_oid,
            change: change.clone(),
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
    )?;
    let new_entries = finish_bulk_write_jobs(
        repo,
        rules,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        write_outcomes,
        progress,
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
) -> Result<()> {
    let index: &Index = index_mut;
    let delete_changes: Vec<&DiffEntry> = changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .collect();
    let write_changes: Vec<DiffEntry> = changes
        .iter()
        .filter(|c| matches!(c.status, DiffStatus::Modified | DiffStatus::Added))
        .cloned()
        .collect();
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
    let mut write_jobs: Vec<CheckoutWriteJob> = Vec::with_capacity(write_changes.len());
    for change in &write_changes {
        let Some(path) = change.new_path.as_deref() else {
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        let file_attrs = rules.file_attrs(path, false);
        write_jobs.push(CheckoutWriteJob {
            path: path.to_owned(),
            mode,
            oid: change.new_oid,
            change: change.clone(),
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
        let abs_paths: Vec<PathBuf> = per_file_deletes
            .iter()
            .map(|p| checkout_stored_worktree_abs(work_tree, p))
            .collect();
        try_par_map(&abs_paths, options, 0, |abs| {
            let _ = fs::remove_file(abs);
            Ok(())
        })
        .map_err(|e: ParallelHashError<()>| match e {
            ParallelHashError::Task(()) => {
                Error::PathError("parallel checkout delete failed".into())
            }
        })?;
        deleted_abs_paths.extend(abs_paths);
        parallel_deleted_paths.extend(per_file_deletes.iter().cloned());
    }
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(delete_changes.len());
    for change in &delete_changes {
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
                let abs = checkout_stored_worktree_abs(work_tree, path);
                let _ = fs::remove_file(&abs);
                deleted_abs_paths.push(abs);
            }
        }
        if !path_under_wholesale_directory(path, wholesale_index_prefixes) {
            paths_to_remove.push(checkout_stored_index_path_bytes(path));
        }
        progress.inc(1);
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
    )?;
    let new_entries = finish_bulk_write_jobs(
        repo,
        rules,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        write_outcomes,
        progress,
    )?;

    checkout_commit_index_update(
        repo,
        index_mut,
        wholesale_index_prefixes,
        &paths_to_remove,
        new_entries,
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
    apply: &CheckoutApplyOptions,
) -> Result<()> {
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
        let abs_paths: Vec<PathBuf> = per_file_deletes
            .iter()
            .map(|p| checkout_stored_worktree_abs(work_tree, p))
            .collect();
        try_par_map(&abs_paths, options, 0, |abs| {
            let _ = fs::remove_file(abs);
            Ok(())
        })
        .map_err(|e: ParallelHashError<()>| match e {
            ParallelHashError::Task(()) => {
                Error::PathError("parallel checkout delete failed".into())
            }
        })?;
        deleted_abs_paths.extend(abs_paths);
        parallel_deleted_paths.extend(per_file_deletes.iter().cloned());
    }
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(changes.len());
    for change in changes {
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
                let abs = checkout_stored_worktree_abs(work_tree, path);
                let _ = fs::remove_file(&abs);
                deleted_abs_paths.push(abs);
            }
        }
        if !path_under_wholesale_directory(path, wholesale_index_prefixes) {
            paths_to_remove.push(checkout_stored_index_path_bytes(path));
        }
        progress.inc(1);
    }
    remove_empty_parent_dirs_batch(work_tree, deleted_abs_paths);
    checkout_commit_index_update(
        repo,
        index_mut,
        wholesale_index_prefixes,
        &paths_to_remove,
        Vec::new(),
        true,
        apply.checkout_result_tree,
        apply.defer_index_disk_write,
    )?;
    progress.finish();
    Ok(())
}

fn prefetch_promisor_blobs_for_checkout(repo: &Repository, changes: &[DiffEntry]) -> Result<()> {
    let config = repo.config()?;
    if !crate::promisor::repo_treats_promisor_packs(repo.git_dir(), config.as_ref()) {
        return Ok(());
    }
    if !crate::promisor_fetch::promisor_lazy_fetch_enabled(config.as_ref(), repo.environment()) {
        return Ok(());
    }
    if repo.caches().promisor_fetcher().is_none() {
        return Ok(());
    }
    let odb = repo.odb();
    let mut oids = Vec::new();
    for change in changes {
        if change.status == DiffStatus::Deleted {
            continue;
        }
        let mode = parse_git_mode(&change.new_mode);
        if mode != MODE_REGULAR && mode != MODE_EXECUTABLE && mode != MODE_SYMLINK {
            continue;
        }
        if change.new_oid.is_zero() || odb.exists_local(&change.new_oid) {
            continue;
        }
        oids.push(change.new_oid);
    }
    if oids.is_empty() {
        return Ok(());
    }
    repo.prefetch_promised(&oids)
}

fn checkout_tree_changes_with_progress(
    repo: &Repository,
    changes: &[DiffEntry],
    progress: &mut dyn ProgressSink,
    reuse_index: Option<&mut Index>,
    apply: CheckoutApplyOptions,
    attribute_changes: Option<&[DiffEntry]>,
) -> Result<()> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    prefetch_promisor_blobs_for_checkout(repo, changes)?;

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
        let bulk_rules = if tree_changes_touch_gitattributes(attr_changes) {
            let prospective_index = prospective_index_for_checkout(index_mut, attr_changes);
            WorktreeRules::from_checkout_prospective_index(
                repo,
                &prospective_index,
                config.clone(),
            )?
        } else {
            WorktreeRules::from_parts_index_attributes_only(repo, index_mut, config.clone())?
        };
        let conv = bulk_rules.conversion().clone();

        if bulk_mod_delete_checkout_eligible(changes, &bulk_rules) {
            return checkout_bulk_mod_delete_tree_changes(
                repo,
                work_tree,
                changes,
                index_mut,
                &bulk_rules,
                &config,
                &conv,
                apply,
                precompose,
                ignorecase,
                progress,
            );
        }
        if bulk_write_checkout_eligible(changes, &bulk_rules)
            && (tree_changes_are_all_additions(changes)
                || tree_changes_are_all_modifications(changes))
        {
            return checkout_bulk_added_tree_changes(
                repo,
                work_tree,
                changes,
                index_mut,
                &bulk_rules,
                &config,
                &conv,
                apply,
                precompose,
                ignorecase,
                progress,
            );
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
                if wholesale_index_prefixes.len() >= 2 {
                    let policy = repo.thread_policy();
                    let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
                    let wt = work_tree.to_path_buf();
                    let dirs: Vec<String> = wholesale_index_prefixes.iter().cloned().collect();
                    let removed = try_par_map(&dirs, options, 0, |dir| {
                        let abs = wt.join(dir);
                        Ok(fs::remove_dir_all(&abs)
                            .is_ok()
                            .then_some((dir.clone(), abs)))
                    })
                    .map_err(|e: ParallelHashError<()>| match e {
                        ParallelHashError::Task(()) => {
                            Error::PathError("parallel wholesale directory removal failed".into())
                        }
                    })?;
                    for (dir, abs) in removed.into_iter().flatten() {
                        wholesale_dirs_removed.insert(dir);
                        deleted_abs_paths.push(abs);
                    }
                } else {
                    for dir in &wholesale_index_prefixes {
                        let abs = work_tree.join(dir);
                        if fs::remove_dir_all(&abs).is_ok() {
                            wholesale_dirs_removed.insert(dir.clone());
                            deleted_abs_paths.push(abs);
                        }
                    }
                }
            }
            return checkout_bulk_deleted_tree_changes(
                repo,
                work_tree,
                changes,
                index_mut,
                &wholesale_index_prefixes,
                &wholesale_dirs_removed,
                &mut deleted_abs_paths,
                progress,
                &apply,
            );
        }
    }

    let index: &Index = index_mut;
    let checkout_smudge_ctx = crate::filter_process::checkout_smudge_context(repo);
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(changes.len() / 8);
    let mut deleted_abs_paths: Vec<PathBuf> = Vec::with_capacity(changes.len() / 8);
    let mut dir_cache = LeadingDirCache::with_protect(protect);
    let mut plan: Vec<CheckoutPlanSlot> = Vec::with_capacity(changes.len());
    let mut write_jobs: Vec<CheckoutWriteJob> = Vec::new();
    let mut rules: Option<WorktreeRules> = None;
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
        if wholesale_index_prefixes.len() >= 2 {
            let policy = repo.thread_policy();
            let options = policy.resolve(ParallelOp::Checkout, config.as_ref());
            let wt = work_tree.to_path_buf();
            let dirs: Vec<String> = wholesale_index_prefixes.iter().cloned().collect();
            let removed = try_par_map(&dirs, options, 0, |dir| {
                let abs = wt.join(dir);
                Ok(fs::remove_dir_all(&abs)
                    .is_ok()
                    .then_some((dir.clone(), abs)))
            })
            .map_err(|e: ParallelHashError<()>| match e {
                ParallelHashError::Task(()) => {
                    Error::PathError("parallel wholesale directory removal failed".into())
                }
            })?;
            for (dir, abs) in removed.into_iter().flatten() {
                wholesale_dirs_removed.insert(dir);
                deleted_abs_paths.push(abs);
            }
        } else {
            for dir in &wholesale_index_prefixes {
                let abs = work_tree.join(dir);
                if fs::remove_dir_all(&abs).is_ok() {
                    wholesale_dirs_removed.insert(dir.clone());
                    deleted_abs_paths.push(abs);
                }
            }
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
        let abs_paths: Vec<PathBuf> = per_file_deletes
            .iter()
            .map(|p| checkout_stored_worktree_abs(work_tree, p))
            .collect();
        try_par_map(&abs_paths, options, 0, |abs| {
            let _ = fs::remove_file(abs);
            Ok(())
        })
        .map_err(|e: ParallelHashError<()>| match e {
            ParallelHashError::Task(()) => {
                Error::PathError("parallel checkout delete failed".into())
            }
        })?;
        deleted_abs_paths.extend(abs_paths);
        parallel_deleted_paths.extend(per_file_deletes);
    }

    {
        crate::trace::trace_span!("grit.checkout.plan");
        for change in changes {
            if change.status == DiffStatus::Deleted {
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
                            let abs = checkout_stored_worktree_abs(work_tree, path);
                            let _ = fs::remove_file(&abs);
                            deleted_abs_paths.push(abs);
                        }
                    }
                    if !path_under_wholesale_directory(path, &wholesale_dirs_removed) {
                        paths_to_remove.push(checkout_stored_index_path_bytes(path));
                    }
                }
                progress.inc(1);
                continue;
            }

            let Some(path) = &change.new_path else {
                progress.inc(1);
                continue;
            };
            if rules.is_none() {
                rules = Some(if tree_changes_touch_gitattributes(attr_changes) {
                    let prospective_index = prospective_index_for_checkout(index, attr_changes);
                    WorktreeRules::from_checkout_prospective_index(
                        repo,
                        &prospective_index,
                        config.clone(),
                    )?
                } else {
                    WorktreeRules::from_parts_index_attributes_only(repo, index, config.clone())?
                });
            }
            let rules = rules.as_ref().ok_or_else(|| {
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
                change: change.clone(),
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
                        progress.inc(1);
                    }
                    WriteJobOutcome::Delayed(target) => {
                        pending_delayed.push(target);
                        progress.inc(1);
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
    let bulk_index_rewrite = changes.len() >= 64;
    checkout_commit_index_update(
        repo,
        index_mut,
        &wholesale_dirs_removed,
        &paths_to_remove,
        new_entries,
        bulk_index_rewrite,
        apply.checkout_result_tree,
        apply.defer_index_disk_write,
    )?;
    progress.finish();
    Ok(())
}

#[derive(Clone)]
struct DelayedCheckoutTarget {
    path: String,
    mode: u32,
    oid: ObjectId,
    change: DiffEntry,
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
    let blob = repo.odb().read(oid)?;
    let worktree_bytes =
        smudge_bytes_for_worktree_path(repo, rules, rel_path, mode, &blob.data, oid)?;
    write_to_worktree_cached(
        work_tree,
        rel_path,
        &worktree_bytes,
        mode,
        &mut LeadingDirCache::new(),
    )?;
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
    progress: &mut dyn ProgressSink,
) -> Result<Vec<IndexEntry>> {
    let mut new_entries = Vec::with_capacity(write_outcomes.len());
    let mut pending_delayed = Vec::new();
    for outcome in write_outcomes {
        match outcome {
            WriteJobOutcome::Index(entry) => {
                new_entries.push(entry);
                progress.inc(1);
            }
            WriteJobOutcome::Delayed(target) => {
                pending_delayed.push(target);
                progress.inc(1);
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
                data,
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

fn write_checkout_entry(
    work_tree: &Path,
    rel_path: &str,
    data: &[u8],
    mode: u32,
    change: &DiffEntry,
    dir_cache: &mut LeadingDirCache,
    oid: ObjectId,
) -> Result<IndexEntry> {
    let index_path = crate::repo_relpath::storage_bytes(rel_path).into_owned();
    if mode == MODE_SYMLINK {
        let meta = write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
        return Ok(entry_from_metadata(&meta, &index_path, oid, mode));
    }

    if can_overwrite_regular_in_place(change) {
        dir_cache.ensure_parents(work_tree, rel_path)?;
        let abs_path = crate::repo_relpath::worktree_abs(work_tree, rel_path);
        let old_mode = parse_git_mode(&change.old_mode);
        let mut file = match OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&abs_path)
        {
            Ok(f) => f,
            Err(_) => {
                let meta = write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
                return Ok(entry_from_metadata(&meta, &index_path, oid, mode));
            }
        };
        file.write_all(data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}' during checkout: {e}")))?;
        drop(file);
        apply_in_place_git_mode_transition_at_path(&abs_path, old_mode, mode)?;
        let meta = written_file_symlink_metadata(&abs_path)?;
        return Ok(entry_from_metadata(&meta, &index_path, oid, mode));
    }

    let meta = write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
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
    jobs: &[CheckoutWriteJob],
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let create_mode = git_create_mode_for_index_mode(mode);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(create_mode)
            .open(abs_path)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}': {e}")))?;
        file.write_all(data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}': {e}")))?;
        written_file_metadata(file, abs_path)
    }
    #[cfg(not(unix))]
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(abs_path)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}': {e}")))?;
        file.write_all(data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}': {e}")))?;
        written_file_metadata(file, abs_path)
    }
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

/// Ensure each component of `rel_path`'s parent exists as a real directory.
///
/// Replaces a parent path that is a symlink or regular file (e.g. `D` → `untracked` or `D` as a
/// file) so `mkdir -p` can create `D/A` during checkout (`t2080` force checkout cases).
pub fn prepare_parent_dirs_for_checkout(work_tree: &Path, rel_path: &str) -> Result<()> {
    let mut cache = LeadingDirCache::new();
    cache.ensure_parents(work_tree, rel_path)?;
    let abs_path = crate::repo_relpath::worktree_abs(work_tree, rel_path);
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Error::PathError(format!("creating parent directories for '{rel_path}': {e}"))
        })?;
    }
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
    let rel_bytes = crate::repo_relpath::storage_bytes(rel_path);
    validate_worktree_path(&rel_bytes, mode, dir_cache.protect)?;
    let abs_path = crate::index::worktree_path_from_index_rel(work_tree, &rel_bytes);

    dir_cache.ensure_parents(work_tree, rel_path)?;
    if let Some(parent) = abs_path.parent() {
        if !dir_cache.ready.contains(parent) {
            fs::create_dir_all(parent).map_err(|e| {
                Error::PathError(format!("creating parent directories for '{rel_path}': {e}"))
            })?;
            dir_cache.ready.insert(parent.to_path_buf());
        }
    }

    // Remove existing file/dir/symlink at target path. Use symlink_metadata + is_symlink so we
    // replace symlinked paths (e.g. `D` → `untracked`) before creating a real directory tree.
    if let Ok(meta) = fs::symlink_metadata(&abs_path) {
        if meta.file_type().is_symlink() {
            fs::remove_file(&abs_path)?;
        } else if meta.is_dir() {
            fs::remove_dir_all(&abs_path)?;
        } else {
            fs::remove_file(&abs_path)?;
        }
    }

    if mode == MODE_SYMLINK {
        let target = std::str::from_utf8(data).map_err(|_| {
            Error::PathError(format!("symlink target for '{rel_path}' is not UTF-8"))
        })?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &abs_path)
            .map_err(|e| Error::PathError(format!("creating symlink '{rel_path}': {e}")))?;
        // Windows lacks unprivileged symlink creation; materialise the link as a
        // regular file containing the target path so the worktree stays populated.
        #[cfg(not(unix))]
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&abs_path)
                .map_err(|e| Error::PathError(format!("writing symlink '{rel_path}': {e}")))?;
            file.write_all(target.as_bytes())
                .map_err(|e| Error::PathError(format!("writing symlink '{rel_path}': {e}")))?;
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

/// Check if a pathspec contains glob characters.
pub fn is_glob_pattern(spec: &str) -> bool {
    spec.contains('*') || spec.contains('?') || spec.contains('[')
}

/// Match a path against a simple glob pattern.
/// Supports `*` (any chars except `/`), `?` (any single char except `/`),
/// and character classes `[abc]`.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    glob_matches_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_matches_inner(pattern: &[u8], path: &[u8]) -> bool {
    let mut pi = 0; // pattern index
    let mut si = 0; // string index
    let mut star_pi = usize::MAX;
    let mut star_si = 0;

    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            si += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // "**" matches everything including '/'
                // For simplicity, try matching rest of pattern at every position
                let rest = &pattern[pi + 2..];
                // Skip optional '/' after **
                let rest = if !rest.is_empty() && rest[0] == b'/' {
                    &rest[1..]
                } else {
                    rest
                };
                for i in si..=path.len() {
                    if glob_matches_inner(rest, &path[i..]) {
                        return true;
                    }
                }
                return false;
            }
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'[' {
            // Character class
            pi += 1;
            let negate = pi < pattern.len() && (pattern[pi] == b'!' || pattern[pi] == b'^');
            if negate {
                pi += 1;
            }
            let mut found = false;
            let ch = path[si];
            while pi < pattern.len() && pattern[pi] != b']' {
                if pi + 2 < pattern.len() && pattern[pi + 1] == b'-' {
                    if ch >= pattern[pi] && ch <= pattern[pi + 2] {
                        found = true;
                    }
                    pi += 3;
                } else {
                    if ch == pattern[pi] {
                        found = true;
                    }
                    pi += 1;
                }
            }
            if pi < pattern.len() {
                pi += 1;
            } // skip ']'
            if found == negate {
                // Mismatch in character class
                if star_pi != usize::MAX {
                    pi = star_pi + 1;
                    star_si += 1;
                    si = star_si;
                } else {
                    return false;
                }
            } else {
                si += 1;
            }
        } else if pi < pattern.len() && pattern[pi] == path[si] {
            pi += 1;
            si += 1;
        } else if star_pi != usize::MAX {
            // Backtrack: '*' matches one more character (including '/')
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }

    // Consume trailing '*' or '**' in pattern
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
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
        checkout_between_trees_with_progress(&repo, None, &tree, &mut rec).expect("checkout");

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
        checkout_between_trees_with_progress(&repo, Some(&head), &to_tree, &mut rec)
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
            CheckoutApplyOptions {
                parallel_prepare_blobs: true,
                checkout_parallel_threshold: Some(0),
                trust_wholesale_dir_removal: true,
                preverified_wholesale_dirs: preverified,
                wholesale_safety_precomputed: true,
                skip_bulk_checkout_fast_path: false,
                checkout_result_tree: None,
                defer_index_disk_write: false,
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
            CheckoutApplyOptions {
                parallel_prepare_blobs: true,
                checkout_parallel_threshold: Some(0),
                trust_wholesale_dir_removal: true,
                preverified_wholesale_dirs: preverified,
                wholesale_safety_precomputed: true,
                skip_bulk_checkout_fast_path: false,
                checkout_result_tree: None,
                defer_index_disk_write: false,
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
}
