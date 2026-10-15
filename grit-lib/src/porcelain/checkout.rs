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

use crate::crlf;
use crate::diff::{diff_trees, format_mode, DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::filter_process::{DelayedCheckoutError, DelayedProcessCheckout, FilterSmudgeMeta};
use crate::index::{
    entry_from_metadata, entry_from_stat, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK,
    MODE_REGULAR, MODE_SYMLINK,
};
use crate::objects::{parse_tree, ObjectId, ObjectKind};
use crate::path_icase::paths_equal;
use crate::progress::{NullProgress, ProgressPhase, ProgressSink};
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;

#[path = "checkout_parallel.rs"]
mod checkout_parallel;
use checkout_parallel::{
    execute_write_jobs, is_eligible_for_parallel_checkout, paths_needing_serial_checkout,
    CheckoutPlanSlot, CheckoutWriteJob, WriteJobOutcome,
};

const CHECKOUT_FILE_PROGRESS_BATCH: u64 = 256;

/// Batched file-count progress for the populated-tree checkout path.
struct CheckoutFileProgress<'a> {
    sink: &'a mut dyn ProgressSink,
    pending: u64,
}

impl<'a> CheckoutFileProgress<'a> {
    fn begin_populated(sink: &'a mut dyn ProgressSink) -> Self {
        sink.start(ProgressPhase::CheckingOutFiles, None);
        Self { sink, pending: 0 }
    }

    fn note_files(&mut self, count: u64) {
        self.pending += count;
        while self.pending >= CHECKOUT_FILE_PROGRESS_BATCH {
            self.sink.inc(CHECKOUT_FILE_PROGRESS_BATCH);
            self.pending -= CHECKOUT_FILE_PROGRESS_BATCH;
        }
    }

    fn finish(self) {
        if self.pending > 0 {
            self.sink.inc(self.pending);
        }
        self.sink.finish();
    }
}

/// Populate an empty work tree and index from `tree_oid` without building a full diff.
fn checkout_populated_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    progress: &mut dyn ProgressSink,
) -> Result<()> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;
    let mut index = repo.load_index()?;
    if !index.entries.is_empty() {
        return checkout_between_trees_slow(repo, None, Some(tree_oid), progress);
    }
    let mut file_progress = CheckoutFileProgress::begin_populated(progress);
    let config = repo.config()?;
    let mut prospective_index = index.clone();
    prospective_index.entries = crate::porcelain::merge::tree_to_index_entries(repo, tree_oid, "")?;
    prospective_index.sort();
    let rules = WorktreeRules::from_checkout_prospective_index(repo, &prospective_index, config)?;
    let mut delayed = DelayedProcessCheckout::default();
    let mut pending_delayed: Vec<DelayedCheckoutTarget> = Vec::new();
    let mut new_entries: Vec<IndexEntry> = Vec::new();
    let mut dir_cache = LeadingDirCache::new();
    let mut stack: Vec<(String, ObjectId)> = vec![(String::new(), *tree_oid)];

    while let Some((prefix, oid)) = stack.pop() {
        let tree_obj = repo.odb().read(&oid)?;
        if tree_obj.kind != ObjectKind::Tree {
            return Err(Error::CorruptObject(format!(
                "expected tree object at {oid}, got {:?}",
                tree_obj.kind
            )));
        }
        for entry in parse_tree(&tree_obj.data)? {
            let name = String::from_utf8_lossy(&entry.name);
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let mode = entry.mode;
            if mode == 0o040000 {
                stack.push((path, entry.oid));
                continue;
            }
            if mode == MODE_GITLINK {
                materialize_gitlink_worktree(work_tree, &path, &mut dir_cache)?;
                let abs = work_tree.join(&path);
                let mut entry = entry_from_stat(&abs, path.as_bytes(), entry.oid, MODE_GITLINK)?;
                entry.size = 0;
                new_entries.push(entry);
                file_progress.note_files(1);
                continue;
            }
            let change = DiffEntry {
                status: DiffStatus::Added,
                old_path: None,
                new_path: Some(path.clone()),
                old_mode: "000000".to_owned(),
                new_mode: format_mode(mode),
                old_oid: ObjectId::zero(),
                new_oid: entry.oid,
                score: None,
            };
            let object = repo.odb().read(&entry.oid)?;
            let worktree_bytes = worktree_bytes_from_index_blob(
                repo,
                &rules,
                &path,
                mode,
                &object.data,
                &entry.oid,
                Some(&mut delayed),
            )?;
            let Some(worktree_bytes) = worktree_bytes else {
                pending_delayed.push(DelayedCheckoutTarget {
                    path: path.clone(),
                    mode,
                    oid: entry.oid,
                    change: change.clone(),
                });
                continue;
            };
            let index_entry = write_checkout_entry(
                work_tree,
                &path,
                &worktree_bytes,
                mode,
                &change,
                &mut dir_cache,
                entry.oid,
            )?;
            new_entries.push(index_entry);
            file_progress.note_files(1);
        }
    }

    finish_delayed_checkouts(
        repo,
        &rules,
        work_tree,
        &mut delayed,
        &pending_delayed,
        &mut dir_cache,
        &mut new_entries,
        Some(&mut file_progress),
    )?;

    index.remove_paths_and_insert([], new_entries);
    repo.write_index(&mut index)?;
    file_progress.finish();
    Ok(())
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
        let path_str = String::from_utf8_lossy(path);
        let abs = work_tree.join(path_str.as_ref());
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
                written_by_tree_diff.insert(path.as_bytes().to_vec());
            }
        }
    }

    let index = repo.load_index()?;
    let config = repo.config()?;
    let rules = WorktreeRules::from_parts(repo, &index, config.clone())?;
    let mut delayed = DelayedProcessCheckout::default();
    let mut pending_delayed: Vec<DelayedCheckoutTarget> = Vec::new();
    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(target_entries.len());
    let mut dir_cache = LeadingDirCache::new();

    for te in target_entries {
        let path = String::from_utf8_lossy(&te.path).into_owned();
        let mode = te.mode;
        if mode == MODE_GITLINK {
            if trees_differ && written_by_tree_diff.contains(&te.path) {
                let abs = work_tree.join(&path);
                let mut entry = entry_from_stat(&abs, te.path.as_slice(), te.oid, MODE_GITLINK)?;
                entry.size = 0;
                new_entries.push(entry);
                continue;
            }
            if let Some(existing) = index.get(te.path.as_slice(), 0) {
                if existing.oid == te.oid
                    && existing.mode == MODE_GITLINK
                    && gitlink_worktree_matches_index(&work_tree.join(&path))
                {
                    let abs = work_tree.join(&path);
                    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                    let mut entry =
                        entry_from_metadata(&meta, te.path.as_slice(), te.oid, MODE_GITLINK);
                    entry.size = 0;
                    new_entries.push(entry);
                    continue;
                }
            }
            materialize_gitlink_worktree(work_tree, &path, &mut dir_cache)?;
            let abs = work_tree.join(&path);
            let mut entry = entry_from_stat(&abs, te.path.as_slice(), te.oid, MODE_GITLINK)?;
            entry.size = 0;
            new_entries.push(entry);
            continue;
        }

        if trees_differ && written_by_tree_diff.contains(&te.path) {
            let abs = work_tree.join(&path);
            let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
            new_entries.push(entry_from_metadata(&meta, te.path.as_slice(), te.oid, mode));
            continue;
        }

        if let Some(existing) = index.get(te.path.as_slice(), 0) {
            if crate::diff::path_checkout_skip_blob_write_when_up_to_date(
                repo.odb(),
                repo.git_dir(),
                work_tree,
                &index,
                existing,
                &te.oid,
                mode,
                &path,
                Some(rules.filter_process()),
            )? {
                let abs = work_tree.join(&path);
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
        None,
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
    )
}

/// Options for [`checkout_tree_changes_with_index_opts`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CheckoutApplyOptions {
    /// Read blob ODB objects in parallel when preparing write jobs.
    ///
    /// Only set when every OID is known to exist (e.g. tree-to-tree switch); general checkout
    /// must keep serial prepare so missing blobs flush earlier writes before failing.
    pub parallel_prepare_blobs: bool,
    /// Override `checkout.thresholdForParallelism` for this apply (switch uses a low threshold).
    pub checkout_parallel_threshold: Option<i64>,
}

/// Like [`checkout_tree_changes`], but updates `index` in place instead of loading from disk.
///
/// Callers such as `grit switch` use this after a tree-diff guard pass that already loaded the index.
pub fn checkout_tree_changes_with_index(
    repo: &Repository,
    changes: &[DiffEntry],
    index: &mut Index,
) -> Result<()> {
    checkout_tree_changes_with_index_opts(repo, changes, index, CheckoutApplyOptions::default())
}

/// Like [`checkout_tree_changes_with_index`], with apply-time tuning for trusted callers.
pub(crate) fn checkout_tree_changes_with_index_opts(
    repo: &Repository,
    changes: &[DiffEntry],
    index: &mut Index,
    apply: CheckoutApplyOptions,
) -> Result<()> {
    checkout_tree_changes_with_progress(repo, changes, &mut NullProgress, Some(index), apply)
}

/// Top-level directories whose tracked files are entirely removed by `delete_paths`.
fn wholesale_deleted_directories(index: &Index, delete_paths: &[String]) -> BTreeSet<String> {
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
    let candidate_dirs: BTreeSet<String> = deleted_under.keys().cloned().collect();
    let mut indexed_under: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &index.entries {
        if entry.stage() != 0 || entry.mode == MODE_GITLINK {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&entry.path) else {
            continue;
        };
        let Some((dir, _rest)) = path.split_once('/') else {
            continue;
        };
        if !candidate_dirs.contains(dir) {
            continue;
        }
        *indexed_under.entry(dir.to_owned()).or_default() += 1;
    }
    deleted_under
        .into_iter()
        .filter_map(|(dir, del_count)| {
            indexed_under
                .get(&dir)
                .copied()
                .filter(|indexed| *indexed == del_count && del_count > 0)
                .map(|_| dir)
        })
        .collect()
}

/// True when every file under `work_tree`/`dir` is listed in `delete_paths` (Git keeps untracked/ignored extras).
fn wholesale_directory_safe_to_remove(
    work_tree: &Path,
    dir: &str,
    delete_paths: &BTreeSet<String>,
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
            if !delete_paths
                .iter()
                .any(|del| paths_equal(rel.as_bytes(), del.as_bytes(), ignorecase))
            {
                return false;
            }
        }
    }
    true
}

fn wholesale_deleted_directories_on_disk(
    work_tree: &Path,
    index: &Index,
    delete_paths: &[String],
    ignorecase: bool,
) -> BTreeSet<String> {
    let delete_set: BTreeSet<String> = delete_paths.iter().cloned().collect();
    wholesale_deleted_directories(index, delete_paths)
        .into_iter()
        .filter(|dir| wholesale_directory_safe_to_remove(work_tree, dir, &delete_set, ignorecase))
        .collect()
}

fn path_under_wholesale_directory(path: &str, wholesale_dirs: &BTreeSet<String>) -> bool {
    wholesale_dirs.iter().any(|dir| {
        path == dir.as_str()
            || path
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn checkout_tree_changes_with_progress(
    repo: &Repository,
    changes: &[DiffEntry],
    progress: &mut dyn ProgressSink,
    reuse_index: Option<&mut Index>,
    apply: CheckoutApplyOptions,
) -> Result<()> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    let mut loaded_index;
    let index_mut: &mut Index = if let Some(index) = reuse_index {
        index
    } else {
        loaded_index = repo.load_index()?;
        &mut loaded_index
    };
    let index: &Index = index_mut;
    let config = repo.config()?;
    let ignorecase = config
        .get_bool("core.ignorecase")
        .and_then(|r| r.ok())
        .unwrap_or(false);
    let precompose = repo.pathspec_precompose_enabled();
    let rules = if tree_changes_touch_gitattributes(changes) {
        let prospective_index = prospective_index_for_checkout(index, changes);
        WorktreeRules::from_checkout_prospective_index(repo, &prospective_index, config.clone())?
    } else {
        WorktreeRules::from_parts_index_attributes_only(repo, index, config.clone())?
    };
    let conv = rules.conversion().clone();
    let mut delayed = DelayedProcessCheckout::default();
    let mut pending_delayed: Vec<DelayedCheckoutTarget> = Vec::new();
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(changes.len() / 8);
    let mut deleted_abs_paths: Vec<PathBuf> = Vec::with_capacity(changes.len() / 8);
    let mut dir_cache = LeadingDirCache::new();
    let mut plan: Vec<CheckoutPlanSlot> = Vec::with_capacity(changes.len());
    let mut write_jobs: Vec<CheckoutWriteJob> = Vec::new();

    progress.start(
        ProgressPhase::CheckingOutFiles,
        Some(u64::try_from(changes.len()).unwrap_or(u64::MAX)),
    );

    let delete_paths: Vec<String> = changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .filter_map(|c| c.old_path.clone())
        .collect();
    let wholesale_candidates =
        wholesale_deleted_directories_on_disk(work_tree, index, &delete_paths, ignorecase);
    let mut wholesale_dirs_removed = BTreeSet::new();
    for dir in &wholesale_candidates {
        let abs = work_tree.join(dir);
        if fs::remove_dir_all(&abs).is_ok() {
            wholesale_dirs_removed.insert(dir.clone());
            deleted_abs_paths.push(abs);
        }
    }

    for change in changes {
        if change.status == DiffStatus::Deleted {
            if let Some(path) = &change.old_path {
                let old_mode = parse_git_mode(&change.old_mode);
                if old_mode == MODE_GITLINK
                    || !path_under_wholesale_directory(path, &wholesale_dirs_removed)
                {
                    let abs = work_tree.join(path);
                    if old_mode == MODE_GITLINK {
                        remove_uninitialized_gitlink_worktree(&abs);
                    } else {
                        let _ = fs::remove_file(&abs);
                    }
                    deleted_abs_paths.push(abs);
                }
                paths_to_remove.push(path.as_bytes().to_vec());
            }
            progress.inc(1);
            continue;
        }

        let Some(path) = &change.new_path else {
            progress.inc(1);
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        if mode == MODE_GITLINK {
            if let Some(existing) = index.get(path.as_bytes(), 0) {
                if existing.oid == change.new_oid
                    && existing.mode == MODE_GITLINK
                    && gitlink_worktree_matches_index(&work_tree.join(path))
                {
                    let abs = work_tree.join(path);
                    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                    let mut entry =
                        entry_from_metadata(&meta, path.as_bytes(), change.new_oid, MODE_GITLINK);
                    entry.size = 0;
                    plan.push(CheckoutPlanSlot::Ready(entry));
                    progress.inc(1);
                    continue;
                }
            }
            materialize_gitlink_worktree(work_tree, path, &mut dir_cache)?;
            let abs = work_tree.join(path);
            let mut entry = entry_from_stat(&abs, path.as_bytes(), change.new_oid, MODE_GITLINK)?;
            entry.size = 0;
            plan.push(CheckoutPlanSlot::Ready(entry));
            progress.inc(1);
            continue;
        }
        if let Some(existing) = index.get(path.as_bytes(), 0) {
            if crate::diff::path_checkout_skip_blob_write_when_up_to_date(
                repo.odb(),
                repo.git_dir(),
                work_tree,
                index,
                existing,
                &change.new_oid,
                mode,
                path,
                Some(rules.filter_process()),
            )? {
                let abs = work_tree.join(path);
                let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
                plan.push(CheckoutPlanSlot::Ready(entry_from_metadata(
                    &meta,
                    path.as_bytes(),
                    change.new_oid,
                    mode,
                )));
                progress.inc(1);
                continue;
            }
        }
        let file_attrs = rules.file_attrs(path, false);
        let smudge_meta =
            crate::filter_process::smudge_meta_for_checkout(repo, &change.new_oid.to_string());
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

    for job in &write_jobs {
        dir_cache.ensure_parents(work_tree, &job.path)?;
    }

    let write_outcomes = execute_write_jobs(
        repo,
        &rules,
        &config,
        &conv,
        work_tree,
        &mut dir_cache,
        &mut delayed,
        &write_jobs,
        &parallel_indices,
        apply.parallel_prepare_blobs,
        apply.checkout_parallel_threshold,
    )?;

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

    finish_delayed_checkouts(
        repo,
        &rules,
        work_tree,
        &mut delayed,
        &pending_delayed,
        &mut dir_cache,
        &mut new_entries,
        None,
    )?;

    remove_empty_parent_dirs_batch(work_tree, &deleted_abs_paths);

    drop(rules);
    index_mut.remove_paths_and_insert(paths_to_remove.iter().map(|p| p.as_slice()), new_entries);
    repo.write_index(index_mut)?;
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
pub(crate) fn worktree_bytes_from_index_blob(
    repo: &Repository,
    rules: &WorktreeRules,
    rel_path: &str,
    mode: u32,
    blob: &[u8],
    oid: &ObjectId,
    delayed_checkout: Option<&mut DelayedProcessCheckout>,
) -> Result<Option<Vec<u8>>> {
    if mode == MODE_SYMLINK {
        return Ok(Some(blob.to_vec()));
    }
    let oid_hex = oid.to_string();
    let smudge_meta = crate::filter_process::smudge_meta_for_checkout(repo, &oid_hex);
    let conv = rules.conversion();
    let file_attrs = rules.file_attrs(rel_path, false);
    crlf::convert_to_worktree(
        blob,
        rel_path,
        conv,
        &file_attrs,
        Some(&oid_hex),
        Some(&smudge_meta),
        delayed_checkout,
        Some(rules.filter_process()),
    )
    .map_err(Error::from)
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
    mut file_progress: Option<&mut CheckoutFileProgress<'_>>,
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
            if let Some(fp) = file_progress.as_deref_mut() {
                fp.note_files(1);
            }
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

/// True when `path` is an uninitialized submodule checkout (directory, no nested `.git`).
fn gitlink_worktree_matches_index(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    let git_meta = path.join(".git");
    !(git_meta.is_file() || git_meta.is_dir())
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
    let file_path = work_tree.join(rel_path);
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
    if mode == MODE_SYMLINK {
        let meta = write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
        return Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode));
    }

    if can_overwrite_regular_in_place(change) {
        dir_cache.ensure_parents(work_tree, rel_path)?;
        let abs_path = work_tree.join(rel_path);
        let old_mode = parse_git_mode(&change.old_mode);
        let mut file = match OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&abs_path)
        {
            Ok(f) => f,
            Err(_) => {
                let meta = write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
                return Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode));
            }
        };
        file.write_all(data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}' during checkout: {e}")))?;
        drop(file);
        apply_in_place_git_mode_transition_at_path(&abs_path, old_mode, mode)?;
        let meta = written_file_symlink_metadata(&abs_path)?;
        return Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode));
    }

    let meta = write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
    Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode))
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
}

impl LeadingDirCache {
    pub(crate) fn new() -> Self {
        Self {
            ready: HashSet::new(),
        }
    }

    /// Share parent-directory state from planning so workers do not re-stat prepared paths.
    pub(crate) fn with_ready_from(source: &Self) -> Self {
        Self {
            ready: source.ready.clone(),
        }
    }

    fn ensure_parents(&mut self, work_tree: &Path, rel_path: &str) -> Result<()> {
        use std::path::Component;
        let path = Path::new(rel_path);
        let Some(parent_rel) = path.parent() else {
            return Ok(());
        };
        if parent_rel.as_os_str().is_empty() {
            return Ok(());
        }
        let mut cur = work_tree.to_path_buf();
        for comp in parent_rel.components() {
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
    let abs_path = work_tree.join(rel_path);
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Error::PathError(format!("creating parent directories for '{rel_path}': {e}"))
        })?;
    }
    Ok(())
}

/// Write data to a working tree file, handling symlinks and executable bits.
pub fn write_to_worktree(work_tree: &Path, rel_path: &str, data: &[u8], mode: u32) -> Result<()> {
    write_to_worktree_cached(work_tree, rel_path, data, mode, &mut LeadingDirCache::new())
        .map(|_| ())
}

pub(crate) fn write_to_worktree_cached(
    work_tree: &Path,
    rel_path: &str,
    data: &[u8],
    mode: u32,
    dir_cache: &mut LeadingDirCache,
) -> Result<fs::Metadata> {
    let abs_path = work_tree.join(rel_path);

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
        let delete_set: BTreeSet<String> = delete_paths.iter().cloned().collect();
        assert!(!super::wholesale_directory_safe_to_remove(
            wt,
            "d",
            &delete_set,
            false
        ));
        let wholesale =
            super::wholesale_deleted_directories_on_disk(wt, &index, &delete_paths, false);
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
        assert_eq!(rec.started, vec![(ProgressPhase::CheckingOutFiles, None)]);
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
}
