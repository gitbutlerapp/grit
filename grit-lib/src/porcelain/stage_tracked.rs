//! Stage modifications and deletions of already-tracked paths (`git commit -a` / `-a` staging).
//!
//! Unlike a full `git add`, untracked paths are ignored. Tracked paths whose worktree stat tuple
//! and mode match the index (and are not racy relative to the index file mtime) are trusted without
//! re-reading file contents — Git's `ie_match_stat` fast path.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::{
    entry_is_racy, index_entry_worktree_abs, materialize_worktree_blob, read_submodule_head_oid,
    symlink_target_bytes, worktree_blob_content, SymlinkDirCache, WorktreeBlobContent,
};
use crate::error::{Error, Result};
use crate::hash::Parallelism;
use crate::index::index_file_mtime;
use crate::index::{
    entry_from_metadata, entry_from_stat, worktree_path_from_index_rel, Index, IndexEntry,
    WorktreeModePolicy, MODE_GITLINK,
};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::{WriteOptions, BULK_TRACKED_RESTAGE_LOOSE_WRITE};
pub use crate::porcelain::bulk_index::IndexStagingThresholds;
use crate::precompose_config::effective_core_precomposeunicode_with_config;
use crate::repo::Repository;
use crate::unicode_normalization::resolve_worktree_path_for_staging;
use crate::worktree::{io_with_path, map_worktree_io, WorktreePathOperation};
use crate::worktree_batch::WorktreeBlobReadInput;
use crate::worktree_rules::WorktreeRules;
use crate::worktree_scan::{
    collect_blob_lookup_batches, group_blob_entries_by_dir, group_blob_entries_by_dir_for_paths,
    stat_scan_thread_count, visit_blob_lookup_batches, BlobDiskLookup, BlobDiskStat,
    BlobScanSparseMode, WorktreeBlobScanOptions,
};

/// One-at-a-time restage writes: the loose prefix directory may not exist yet.
const SERIAL_RESTAGE_WRITE: WriteOptions = WriteOptions {
    loose_prefix_dirs_ready: false,
    ..BULK_TRACKED_RESTAGE_LOOSE_WRITE
};

/// Most deferred restage jobs read into memory at once; bounds peak RSS on bulk restages.
const RESTAGE_CHUNK_MAX_JOBS: usize = 1024;

/// Most worktree bytes (by stat size) one deferred restage chunk reads into memory at once.
const RESTAGE_CHUNK_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Summary of paths updated while staging tracked modifications/deletions.
///
/// Paths are stored as raw index bytes (may be non-UTF-8 on Unix).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageTrackedSummary {
    /// Tracked paths newly staged or re-staged with updated blob/mode metadata.
    pub modified: Vec<Vec<u8>>,
    /// Tracked paths removed from the index because they disappeared from the worktree.
    pub removed: Vec<Vec<u8>>,
    /// Per-path UNTR invalidation was deferred during this scan (campaign-scale restage).
    pub untracked_invalidation_deferred: bool,
}

impl StageTrackedSummary {
    /// Total number of index rows added, updated, or removed.
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.modified.len() + self.removed.len()
    }
}

/// Stage worktree modifications and deletions for every tracked path in the index.
///
/// This is the engine behind `git commit -a`: refresh tracked files from disk, drop deleted
/// paths, collapse unmerged paths to a single stage-0 row when the worktree file is present,
/// and skip content re-hashing when stat data proves the blob is unchanged (unless the entry is
/// racy relative to the on-disk index mtime).
///
/// Untracked files are not added. [`IndexEntry::skip_worktree`] entries are left unchanged; missing paths
/// are not treated as deletions for those entries.
///
/// The index is read and written while holding `index.lock`, so concurrent index writers are
/// never overwritten with a stale copy.
///
/// # Parameters
/// - `repo` — repository handle (object store and index paths).
/// - `work_tree` — working tree root.
///
/// # Returns
/// A [`StageTrackedSummary`] listing modified and removed index paths (sorted).
///
/// # Errors
/// [`Error::LockHeld`] when another process holds `index.lock` past the
/// `core.filesRefLockTimeout` wait, I/O failures, object write failures, or a missing index
/// when one is required.
pub fn stage_tracked_modifications(
    repo: &Repository,
    work_tree: &Path,
) -> Result<StageTrackedSummary> {
    stage_tracked_modifications_with_thresholds(repo, work_tree, IndexStagingThresholds::default())
}

/// Like [`stage_tracked_modifications`], with explicit bulk/parallel entry-count thresholds.
///
/// # Errors
///
/// Same as [`stage_tracked_modifications`].
pub fn stage_tracked_modifications_with_thresholds(
    repo: &Repository,
    work_tree: &Path,
    thresholds: IndexStagingThresholds,
) -> Result<StageTrackedSummary> {
    let index_path = repo.index_path_for_env()?;
    let (mut lock, mut index) = match repo.lock_and_load_index() {
        Ok(locked) => locked,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StageTrackedSummary::default());
        }
        Err(e) => return Err(e),
    };

    let summary = stage_tracked_modifications_in_index_with_thresholds(
        repo,
        work_tree,
        &index_path,
        &mut index,
        thresholds,
    )?;

    if summary.untracked_invalidation_deferred {
        replay_deferred_untracked_invalidations(&mut index, &summary);
    }
    if summary.change_count() > 0 {
        index.ensure_canonical_order();
        repo.write_index_with_lock(&mut lock, &mut index, false)?;
    }

    Ok(summary)
}

/// Like [`stage_tracked_modifications`], but mutates a caller-owned index without writing it.
///
/// Useful when the caller will merge further index updates before a single write (for example
/// `git commit --dry-run` or `grit add` after a tracked scan).
///
/// When [`StageTrackedSummary::untracked_invalidation_deferred`] is true, call
/// [`replay_deferred_untracked_invalidations`] on the same index before persisting it (as
/// [`stage_tracked_modifications`] does automatically).
///
/// # Parameters
/// - `repo` — repository handle (object store).
/// - `work_tree` — working tree root.
/// - `index_path` — on-disk index path used to sample racy-timestamp context.
/// - `index` — index to update in place.
///
/// # Returns
/// A [`StageTrackedSummary`] with sorted `modified` and `removed` path lists.
///
/// # Errors
/// I/O failures or object write failures while reading the worktree.
pub fn stage_tracked_modifications_in_index(
    repo: &Repository,
    work_tree: &Path,
    index_path: &Path,
    index: &mut Index,
) -> Result<StageTrackedSummary> {
    stage_tracked_modifications_in_index_with_thresholds(
        repo,
        work_tree,
        index_path,
        index,
        IndexStagingThresholds::default(),
    )
}

/// Like [`stage_tracked_modifications_in_index`], with explicit staging thresholds.
pub fn stage_tracked_modifications_in_index_with_thresholds(
    repo: &Repository,
    work_tree: &Path,
    index_path: &Path,
    index: &mut Index,
    thresholds: IndexStagingThresholds,
) -> Result<StageTrackedSummary> {
    let mut rules = WorktreeRules::from_repository(repo, index)?;
    stage_tracked_modifications_in_index_with_rules(
        repo, work_tree, index_path, index, &mut rules, thresholds, None,
    )
}

/// Like [`stage_tracked_modifications_in_index`], reusing an already-built [`WorktreeRules`].
///
/// When `restrict_to_paths` is set, only those stage-0 index paths are stat-scanned (explicit
/// `grit add` pathspecs on large trees).
pub(crate) fn stage_tracked_modifications_in_index_with_rules(
    repo: &Repository,
    work_tree: &Path,
    index_path: &Path,
    index: &mut Index,
    rules: &mut WorktreeRules,
    thresholds: IndexStagingThresholds,
    restrict_to_paths: Option<&BTreeSet<String>>,
) -> Result<StageTrackedSummary> {
    let entry_count_at_start = index.entries.len();
    let bulk_untr_defer = entry_count_at_start >= thresholds.large_index_restage;
    let parallel_restage = entry_count_at_start >= thresholds.parallel_tracked_restage;
    if bulk_untr_defer {
        index.defer_untracked_cache_invalidation = true;
    }

    let result = stage_tracked_modifications_in_index_body(
        repo,
        work_tree,
        index_path,
        index,
        bulk_untr_defer,
        parallel_restage,
        rules,
        restrict_to_paths,
    );
    if bulk_untr_defer {
        index.defer_untracked_cache_invalidation = false;
    }
    result
}

/// Replay UNTR invalidations suppressed during a bulk tracked restage.
///
/// Required before writing the index when [`StageTrackedSummary::untracked_invalidation_deferred`]
/// is true (see [`stage_tracked_modifications_in_index`]).
pub fn replay_deferred_untracked_invalidations(index: &mut Index, summary: &StageTrackedSummary) {
    if !summary.untracked_invalidation_deferred {
        return;
    }
    if summary.removed.is_empty() {
        // Tracked blob edits alone do not change which paths are untracked; skip UNTR work so
        // commit/add hot paths can reuse a warm untracked cache (issue #1064).
        return;
    }
    for raw in &summary.removed {
        let path = std::str::from_utf8(raw)
            .map_or_else(|_| String::from_utf8_lossy(raw).into_owned(), str::to_owned);
        index.invalidate_untracked_cache_for_path(&path);
    }
    // Per-path invalidation is a no-op when UNTR has no directory node for a path; tracked-set
    // removals during bulk defer still require marking the root invalid (Git invalidates UNTR).
    if let Some(uc) = index.untracked_cache.as_mut() {
        if let Some(root) = uc.root.as_mut() {
            root.valid = false;
        }
    }
}

/// When `restrict` is `Some`, only paths in the set are scanned (including empty = no tracked scan).
fn tracked_scan_includes_path(restrict: Option<&BTreeSet<String>>, raw_path: &[u8]) -> bool {
    let Some(set) = restrict else {
        return true;
    };
    let path = std::str::from_utf8(raw_path)
        .map(str::to_owned)
        .unwrap_or_else(|_| String::from_utf8_lossy(raw_path).into_owned());
    set.contains(&path)
}

#[allow(clippy::too_many_arguments)]
fn stage_tracked_modifications_in_index_body(
    repo: &Repository,
    work_tree: &Path,
    index_path: &Path,
    index: &mut Index,
    bulk_untr_defer: bool,
    parallel_restage: bool,
    rules: &mut WorktreeRules,
    restrict_to_paths: Option<&BTreeSet<String>>,
) -> Result<StageTrackedSummary> {
    let index_mtime = index_file_mtime(index_path);
    let precompose_unicode = effective_core_precomposeunicode_with_config(
        Some(repo.git_dir()),
        repo.config().ok().as_deref(),
    );
    let mode_policy = repo
        .config()
        .map(|cfg| WorktreeModePolicy::from_config(&cfg))
        .unwrap_or_default();

    let mut unmerged_paths: HashSet<Vec<u8>> = HashSet::new();
    let mut special_stage0: Vec<(Vec<u8>, IndexEntry)> = Vec::new();
    for e in &index.entries {
        if e.stage() != 0 {
            unmerged_paths.insert(e.path.clone());
        } else if e.mode == MODE_GITLINK
            || e.intent_to_add()
            || std::str::from_utf8(&e.path).is_err()
        {
            special_stage0.push((e.path.clone(), e.clone()));
        }
    }

    let mut symlink_parent_cache: HashMap<PathBuf, bool> = HashMap::new();
    let mut summary = StageTrackedSummary {
        untracked_invalidation_deferred: bulk_untr_defer,
        ..StageTrackedSummary::default()
    };
    let ignorecase = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);
    let icase_collisions = if ignorecase {
        Some(crate::path_icase::IcaseCheckoutCollisionMap::from_index(
            work_tree, index, true,
        ))
    } else {
        None
    };
    let dir_symlinks = SymlinkDirCache::default();

    for raw_path in &unmerged_paths {
        if !tracked_scan_includes_path(restrict_to_paths, raw_path) {
            continue;
        }
        let abs_path =
            abs_path_for_stage_tracked(work_tree, raw_path, precompose_unicode, ignorecase);
        refresh_unmerged_tracked_path(
            repo,
            index,
            raw_path,
            &abs_path,
            mode_policy,
            rules,
            &mut summary,
        )?;
    }

    for (raw_path, idx_e) in &special_stage0 {
        if !tracked_scan_includes_path(restrict_to_paths, raw_path) {
            continue;
        }
        stage_tracked_path_individual(
            repo,
            work_tree,
            index,
            raw_path,
            idx_e,
            index_mtime,
            mode_policy,
            precompose_unicode,
            icase_collisions.as_ref(),
            &mut symlink_parent_cache,
            rules,
            &mut summary,
        )?;
    }

    let rules_config = repo
        .config()
        .unwrap_or_else(|_| std::sync::Arc::new(crate::config::ConfigSet::new()));
    let mut honor_fsmonitor = false;
    if let Ok(cfg) = repo.config() {
        let default_runner = crate::command_runner::system_command_runner();
        let boundary = crate::fsmonitor::FsmonitorQueryBoundary(
            crate::environment::wall_clock_unix_timestamp_nanos(),
        );
        let refresh_opts = crate::fsmonitor::FsmonitorRefreshOptions::with_query_boundary(boundary);
        let refresh = crate::fsmonitor::refresh_index(
            index,
            repo.git_dir(),
            work_tree,
            cfg.as_ref(),
            Some(default_runner.as_ref()),
            &refresh_opts,
        )?;
        honor_fsmonitor = refresh.honor_valid_bits;
    }
    let blob_dirs = match restrict_to_paths {
        Some(paths) => group_blob_entries_by_dir_for_paths(index, paths),
        None => group_blob_entries_by_dir(index, honor_fsmonitor),
    };
    let work_tree_owned = work_tree.to_path_buf();
    let dir_abs = move |dir: &[u8]| {
        if dir.is_empty() {
            work_tree_owned.clone()
        } else {
            crate::diff::index_entry_worktree_abs_bytes(
                &work_tree_owned,
                dir,
                precompose_unicode,
                ignorecase,
            )
        }
    };
    let work_tree_for_files = work_tree.to_path_buf();
    let work_tree_for_symlink = work_tree.to_path_buf();
    let file_abs = move |rel: &[u8]| {
        crate::diff::index_entry_worktree_abs_bytes(
            &work_tree_for_files,
            rel,
            precompose_unicode,
            ignorecase,
        )
    };
    let preload_index = rules_config
        .get_bool("core.preloadIndex")
        .and_then(|r| r.ok())
        .unwrap_or(true);
    let blob_count = blob_dirs.len();
    let thread_policy = repo.thread_policy();
    let stat_threads = stat_scan_thread_count(
        &thread_policy,
        rules_config.as_ref(),
        WorktreeBlobScanOptions {
            preload_index,
            stat_parallel_threads: None,
        },
        blob_count,
    );
    let blob_scan_opts = WorktreeBlobScanOptions {
        preload_index,
        stat_parallel_threads: Some(stat_threads),
    };
    let parallelism = thread_policy
        .resolve(
            crate::parallel::ParallelOp::IndexRefresh,
            rules_config.as_ref(),
        )
        .parallelism();
    let mut deferred_restage: Vec<DeferredRestageJob> = Vec::new();
    let mut pending_removals: Vec<Vec<u8>> = Vec::new();
    let symlink_parent_cache = std::sync::Mutex::new(symlink_parent_cache);
    let stage_batches = collect_blob_lookup_batches(
        index,
        &blob_dirs,
        blob_scan_opts,
        &thread_policy,
        &rules_config,
        dir_abs,
        file_abs,
        |rel_path| {
            dir_symlinks.has_symlink_in_path_bytes(
                work_tree,
                rel_path,
                precompose_unicode,
                ignorecase,
            ) || {
                let abs = crate::diff::index_entry_worktree_abs_bytes(
                    &work_tree_for_symlink,
                    rel_path,
                    precompose_unicode,
                    ignorecase,
                );
                let mut cache = symlink_parent_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                path_has_symlink_parent_cached(work_tree, &abs, &mut cache)
            }
        },
        BlobScanSparseMode::StageTracked {
            index_mtime,
            mode_policy,
        },
    )?;
    visit_blob_lookup_batches(stage_batches, |entry_index, lookup| {
        let raw_path = index.entries[entry_index].path.clone();
        let rel_path = crate::repo_relpath::storage_string(&raw_path);
        let rel_path = rel_path.as_str();
        let Some(idx_e) = index.get(&raw_path, 0).cloned() else {
            return Ok(());
        };
        if idx_e.stage() != 0 {
            return Ok(());
        }
        let abs_path =
            abs_path_for_stage_tracked(work_tree, &raw_path, precompose_unicode, ignorecase);
        match lookup {
            BlobDiskLookup::Missing => {
                if idx_e.skip_worktree() {
                    return Ok(());
                }
                if icase_collisions
                    .as_ref()
                    .is_some_and(|map| map.preserve_missing_collision_loser(raw_path.as_slice()))
                {
                    return Ok(());
                }
                if rel_path_has_symlink_parent(work_tree, rel_path) {
                    pending_removals.push(raw_path);
                    return Ok(());
                }
                if symlink_metadata_for_staging(rel_path, &abs_path)?.is_none() {
                    pending_removals.push(raw_path);
                }
            }
            BlobDiskLookup::Io(e) => {
                return Err(map_worktree_io(
                    rel_path,
                    WorktreePathOperation::Metadata,
                    e,
                ));
            }
            BlobDiskLookup::Present(stat) => {
                let ctx = PresentRefreshCtx {
                    repo,
                    index,
                    raw_path: &raw_path,
                    abs_path: &abs_path,
                    idx_e: &idx_e,
                    index_mtime,
                    mode_policy,
                    rules,
                    icase_collisions: icase_collisions.as_ref(),
                };
                let defer = parallel_restage.then_some(&mut deferred_restage);
                if refresh_present_tracked_path(ctx, stat, defer, entry_index)? {
                    summary.modified.push(raw_path);
                }
            }
        }
        Ok(())
    })?;
    for raw_path in pending_removals {
        if index.remove(&raw_path) {
            summary.removed.push(raw_path);
        }
    }

    if !deferred_restage.is_empty() {
        let restage_ctx = DeferredRestageContext {
            work_tree,
            precompose_unicode,
            ignorecase,
            icase_collisions: icase_collisions.as_ref(),
        };
        summary.modified.extend(restage_deferred_jobs(
            repo,
            index,
            rules,
            mode_policy,
            parallelism,
            restage_ctx,
            &deferred_restage,
        )?);
    }

    summary.modified.sort();
    summary.removed.sort();
    Ok(summary)
}

struct PresentRefreshCtx<'a> {
    repo: &'a Repository,
    index: &'a mut Index,
    raw_path: &'a [u8],
    abs_path: &'a Path,
    idx_e: &'a IndexEntry,
    index_mtime: Option<(u32, u32)>,
    mode_policy: WorktreeModePolicy,
    rules: &'a mut WorktreeRules,
    icase_collisions: Option<&'a crate::path_icase::IcaseCheckoutCollisionMap>,
}

fn refresh_unmerged_tracked_path(
    repo: &Repository,
    index: &mut Index,
    raw_path: &[u8],
    abs_path: &Path,
    mode_policy: WorktreeModePolicy,
    rules: &mut WorktreeRules,
    summary: &mut StageTrackedSummary,
) -> Result<()> {
    let idx_mode = index
        .entries
        .iter()
        .find(|e| e.path == raw_path && e.stage() == 0)
        .map(|e| e.mode)
        .or_else(|| {
            index
                .entries
                .iter()
                .find(|e| e.path == raw_path)
                .map(|e| e.mode)
        })
        .unwrap_or(0o100644);
    let index_relpath = crate::repo_relpath::storage_string(raw_path);
    match symlink_metadata_for_staging(&index_relpath, abs_path)? {
        Some(meta) => {
            if crate::rerere::worktree_metadata_needs_conflict_marker_inspection(&meta, idx_mode) {
                let wt_bytes = io_with_path(
                    &index_relpath,
                    WorktreePathOperation::Read,
                    fs::read(abs_path),
                )?;
                let marker_size = std::str::from_utf8(raw_path)
                    .map(|rel| {
                        rules
                            .file_attrs(rel, false)
                            .effective_conflict_marker_size()
                    })
                    .unwrap_or(7);
                if crate::rerere::worktree_bytes_have_unresolved_conflict_markers(
                    &wt_bytes,
                    marker_size,
                ) {
                    return Ok(());
                }
            }
            index.remove(raw_path);
            if idx_mode == MODE_GITLINK {
                if let Some(oid) = read_submodule_head_oid(abs_path) {
                    stage_gitlink_from_stat(abs_path, raw_path, oid, index)?;
                    summary.modified.push(raw_path.to_vec());
                }
            } else {
                let mode = mode_policy.index_mode_for_worktree(
                    crate::diff::mode_from_metadata(&meta),
                    Some(idx_mode),
                );
                stage_blob_from_worktree(repo, index, abs_path, raw_path, Some(mode), rules)?;
                summary.modified.push(raw_path.to_vec());
            }
        }
        None => {
            // A missing worktree file for an unmerged path is expected for modify/delete
            // conflicts and while the user is resolving; do not drop merge stages.
        }
    }
    Ok(())
}

/// Worktree metadata for staging: present, definitively missing, or I/O error.
fn symlink_metadata_for_staging(
    index_relpath: &str,
    abs_path: &Path,
) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(abs_path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if worktree_path_is_missing(&e) => Ok(None),
        Err(e) => Err(map_worktree_io(
            index_relpath,
            WorktreePathOperation::Metadata,
            e,
        )),
    }
}

fn worktree_path_is_missing(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::NotFound)
}

fn abs_path_for_stage_tracked(
    work_tree: &Path,
    raw_path: &[u8],
    precompose_unicode: bool,
    ignorecase: bool,
) -> PathBuf {
    let Some(rel) = std::str::from_utf8(raw_path).ok() else {
        return worktree_path_from_index_rel(work_tree, raw_path);
    };
    if precompose_unicode {
        return resolve_worktree_path_for_staging(work_tree, rel, true).abs;
    }
    index_entry_worktree_abs(work_tree, rel, false, ignorecase)
}

#[allow(clippy::too_many_arguments)]
fn stage_tracked_path_individual(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    raw_path: &[u8],
    idx_e: &IndexEntry,
    index_mtime: Option<(u32, u32)>,
    mode_policy: WorktreeModePolicy,
    precompose_unicode: bool,
    icase_collisions: Option<&crate::path_icase::IcaseCheckoutCollisionMap>,
    symlink_parent_cache: &mut HashMap<PathBuf, bool>,
    rules: &mut WorktreeRules,
    summary: &mut StageTrackedSummary,
) -> Result<()> {
    let ignorecase = icase_collisions.is_some();
    let abs_path = abs_path_for_stage_tracked(work_tree, raw_path, precompose_unicode, ignorecase);
    if path_has_symlink_parent_cached(work_tree, &abs_path, symlink_parent_cache) {
        if index.remove(raw_path) {
            summary.removed.push(raw_path.to_vec());
        }
        return Ok(());
    }
    let index_relpath = crate::repo_relpath::storage_string(raw_path);
    match symlink_metadata_for_staging(&index_relpath, &abs_path)? {
        Some(meta) => {
            let ctx = PresentRefreshCtx {
                repo,
                index,
                raw_path,
                abs_path: &abs_path,
                idx_e,
                index_mtime,
                mode_policy,
                rules,
                icase_collisions,
            };
            if refresh_present_tracked_path(ctx, BlobDiskStat::from_metadata(&meta), None, 0)? {
                summary.modified.push(raw_path.to_vec());
            }
        }
        None if idx_e.skip_worktree() => {}
        None => {
            if icase_collisions.is_some_and(|map| map.preserve_missing_collision_loser(raw_path)) {
                return Ok(());
            }
            if index.remove(raw_path) {
                summary.removed.push(raw_path.to_vec());
            }
        }
    }
    Ok(())
}

/// Deferred blob restage: path captured at defer time (stable if later index rows shift).
struct DeferredRestageJob {
    raw_path: Vec<u8>,
    /// Worktree size from the scan's stat, used only to bound chunk memory.
    size: u64,
}

struct DeferredRestageContext<'a> {
    work_tree: &'a Path,
    precompose_unicode: bool,
    ignorecase: bool,
    icase_collisions: Option<&'a crate::path_icase::IcaseCheckoutCollisionMap>,
}

const S_IFMT: u32 = 0o170000;

fn git_entry_type(mode: u32) -> u32 {
    mode & S_IFMT
}

fn refresh_present_tracked_path(
    ctx: PresentRefreshCtx<'_>,
    stat: BlobDiskStat,
    defer_restage: Option<&mut Vec<DeferredRestageJob>>,
    entry_index: usize,
) -> Result<bool> {
    let PresentRefreshCtx {
        repo,
        index,
        raw_path,
        abs_path,
        idx_e,
        index_mtime,
        mode_policy,
        rules,
        icase_collisions,
    } = ctx;
    if icase_collisions.is_some_and(|map| map.is_auto_stage_loser(raw_path)) {
        return Ok(false);
    }
    let idx_mode = idx_e.mode;
    let idx_intent_to_add = idx_e.intent_to_add();
    let idx_oid = idx_e.oid;

    if idx_mode == 0o160000 && stat.is_dir {
        return refresh_gitlink(repo, index, raw_path, abs_path, idx_e);
    }
    // Gitlink in the index but a non-directory in the work tree (e.g. regular file): stage as blob below.

    if stat.is_dir {
        if abs_path.join(".git").exists() {
            if let Some(oid) = read_submodule_head_oid(abs_path) {
                if index
                    .entries
                    .iter()
                    .find(|e| e.path == *raw_path)
                    .is_some_and(|e| e.oid == oid && e.mode == 0o160000)
                {
                    return Ok(false);
                }
                stage_gitlink_from_stat(abs_path, raw_path, oid, index)?;
                return Ok(true);
            }
        } else {
            index.remove(raw_path);
            return Ok(true);
        }
    }

    let wt_mode = mode_policy.index_mode_for_worktree(stat.git_mode, Some(idx_mode));
    let type_changed = git_entry_type(wt_mode) != git_entry_type(idx_mode);
    let stat_same = stat
        .fields
        .matches_index_entry(idx_e, crate::index::StatMatchPolicy::default());
    let racy = entry_is_racy(idx_e, index_mtime);
    if !idx_intent_to_add && idx_e.size != 0 && stat_same && !racy {
        if wt_mode == idx_mode {
            return Ok(false);
        }
        // A type change (blob ↔ symlink, etc.) falls through to restage the content.
        if !type_changed {
            let entry = entry_from_stat(abs_path, raw_path, idx_oid, wt_mode)?;
            index.stage_file(entry);
            return Ok(true);
        }
    }

    let oid = match std::str::from_utf8(raw_path) {
        Ok(rel_path) => {
            if let Some(deferred) = defer_restage {
                let _ = entry_index;
                deferred.push(DeferredRestageJob {
                    raw_path: raw_path.to_vec(),
                    size: u64::from(stat.fields.index_size()),
                });
                return Ok(false);
            }
            let meta = fs::symlink_metadata(abs_path).map_err(Error::Io)?;
            let file_attrs = rules.file_attrs(rel_path, false);
            let content = worktree_blob_content(
                repo.odb(),
                abs_path,
                &meta,
                rules.conversion(),
                &file_attrs,
                rel_path,
                Some(idx_e),
                Some(rules.filter_process()),
            )?;
            enum RestagePayload {
                Bytes(Vec<u8>),
                Stream { file: fs::File, size: u64 },
                Stored,
            }
            let (staged_oid, payload) = match content {
                WorktreeBlobContent::Bytes(data) => {
                    let staged_oid = repo.odb().hash(ObjectKind::Blob, &data)?;
                    (staged_oid, RestagePayload::Bytes(data))
                }
                WorktreeBlobContent::Stream { mut file, size }
                    if idx_intent_to_add
                        || (size != u64::from(idx_e.size)
                            && repo
                                .odb()
                                .read_info_without_fetch(&idx_oid)
                                .is_ok_and(|info| info.size != size)) =>
                {
                    // The canonical blob size proves these bytes cannot match the index.
                    // Store and hash together; unchanged streams still take the hash-only
                    // path below, without creating another loose object or pack.
                    let oid = repo.odb().write_stream(
                        ObjectKind::Blob,
                        size,
                        &mut file,
                        SERIAL_RESTAGE_WRITE,
                    )?;
                    (oid, RestagePayload::Stored)
                }
                WorktreeBlobContent::Stream { mut file, size } => {
                    let staged_oid = repo.odb().hash_stream(ObjectKind::Blob, size, &mut file)?;
                    use std::io::{Seek, SeekFrom};
                    file.seek(SeekFrom::Start(0)).map_err(|source| {
                        map_worktree_io(rel_path, WorktreePathOperation::Read, source)
                    })?;
                    (staged_oid, RestagePayload::Stream { file, size })
                }
            };
            // Stat taken before the read, so a write racing the read is caught next time.
            let entry = entry_from_metadata(&meta, raw_path, staged_oid, wt_mode);
            if !idx_intent_to_add && staged_oid == idx_oid {
                if wt_mode != idx_mode || stat_fields_differ(&entry, idx_e) {
                    index.stage_file(entry);
                    index.mark_content_verified(raw_path, staged_oid);
                    return Ok(true);
                }
                return Ok(false);
            }
            match payload {
                RestagePayload::Stored => {}
                RestagePayload::Bytes(data) => {
                    repo.odb()
                        .write_blob_prehashed(&staged_oid, &data, SERIAL_RESTAGE_WRITE)?;
                }
                RestagePayload::Stream { mut file, size } => {
                    repo.odb().write_stream(
                        ObjectKind::Blob,
                        size,
                        &mut file,
                        SERIAL_RESTAGE_WRITE,
                    )?;
                }
            }
            index.stage_file(entry);
            index.mark_content_verified(raw_path, staged_oid);
            return Ok(true);
        }
        Err(_) => {
            let meta = fs::symlink_metadata(abs_path).map_err(Error::Io)?;
            let oid = read_worktree_blob_oid_raw(repo, abs_path, &meta)?;
            if !idx_intent_to_add && idx_oid == oid && wt_mode == idx_mode {
                return Ok(false);
            }
            oid
        }
    };
    let entry = entry_from_stat(abs_path, raw_path, oid, wt_mode)?;
    index.stage_file(entry);
    Ok(true)
}

fn restage_deferred_jobs(
    repo: &Repository,
    index: &mut Index,
    rules: &mut WorktreeRules,
    mode_policy: WorktreeModePolicy,
    parallelism: Parallelism,
    ctx: DeferredRestageContext<'_>,
    jobs: &[DeferredRestageJob],
) -> Result<Vec<Vec<u8>>> {
    let mut modified = Vec::new();
    let mut loose_dirs_ready = false;
    for chunk in restage_chunks(jobs) {
        let replacements = restage_deferred_chunk(
            repo,
            index,
            rules,
            mode_policy,
            parallelism,
            &ctx,
            chunk,
            &mut loose_dirs_ready,
            &mut modified,
        )?;
        for entry in &replacements {
            index.mark_content_verified(&entry.path, entry.oid);
        }
        index.overwrite_stage0_entries(replacements);
    }
    Ok(modified)
}

/// Split `jobs` into runs of at most [`RESTAGE_CHUNK_MAX_JOBS`] jobs and roughly
/// [`RESTAGE_CHUNK_MAX_BYTES`] worktree bytes (a single larger file is its own chunk).
fn restage_chunks(jobs: &[DeferredRestageJob]) -> Vec<&[DeferredRestageJob]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0u64;
    for (i, job) in jobs.iter().enumerate() {
        let len = i - start;
        if len > 0
            && (len >= RESTAGE_CHUNK_MAX_JOBS
                || bytes.saturating_add(job.size) > RESTAGE_CHUNK_MAX_BYTES)
        {
            chunks.push(&jobs[start..i]);
            start = i;
            bytes = 0;
        }
        bytes = bytes.saturating_add(job.size);
    }
    if start < jobs.len() {
        chunks.push(&jobs[start..]);
    }
    chunks
}

/// Read, hash and store one chunk of deferred jobs in parallel. Appends the restaged paths
/// to `modified` and returns the index rows to apply; `index` is not changed yet, so the
/// blob store pass still compares against the rows being replaced.
#[allow(clippy::too_many_arguments)]
fn restage_deferred_chunk(
    repo: &Repository,
    index: &Index,
    rules: &mut WorktreeRules,
    mode_policy: WorktreeModePolicy,
    parallelism: Parallelism,
    ctx: &DeferredRestageContext<'_>,
    jobs: &[DeferredRestageJob],
    loose_dirs_ready: &mut bool,
    modified: &mut Vec<Vec<u8>>,
) -> Result<Vec<IndexEntry>> {
    let mut replacements = Vec::with_capacity(jobs.len());
    let &DeferredRestageContext {
        work_tree,
        precompose_unicode,
        ignorecase,
        icase_collisions,
    } = ctx;
    let mut batch_inputs = Vec::with_capacity(jobs.len());
    for job in jobs {
        let raw_path = job.raw_path.as_slice();
        let rel_path = std::str::from_utf8(raw_path)
            .map_err(|_| Error::Message("deferred restage requires utf-8 index paths".into()))?;
        let Some(idx_e) = index.get(raw_path, 0).cloned() else {
            continue;
        };
        let abs_path =
            abs_path_for_stage_tracked(work_tree, raw_path, precompose_unicode, ignorecase);
        let file_attrs = rules.file_attrs(rel_path, false);
        let index_entry =
            if crate::crlf::clean_uses_autocrlf_index_guard(&file_attrs, rules.conversion()) {
                Some(idx_e)
            } else {
                None
            };
        batch_inputs.push(WorktreeBlobReadInput {
            abs: abs_path,
            index_relpath: rel_path.to_owned(),
            index_entry,
            file_attrs,
            cached_meta: None,
        });
    }
    let prepared = crate::worktree_batch::prepare_worktree_blobs_parallel(
        repo.odb(),
        &batch_inputs,
        rules.conversion(),
        parallelism,
        Some(rules.filter_process()),
    )?;

    let mut needs_loose_write = false;
    for (job, prep) in jobs.iter().zip(prepared.iter()) {
        let raw_path = job.raw_path.as_slice();
        let Some(idx_e) = index.get(raw_path, 0) else {
            continue;
        };
        debug_assert_eq!(
            std::str::from_utf8(raw_path).ok(),
            Some(prep.blob.index_relpath.as_str())
        );
        let mode_for_index = mode_policy.index_mode_for_worktree(prep.blob.mode, Some(idx_e.mode));
        // The read pass's stat precedes the read, like the serial path.
        let entry = entry_from_metadata(&prep.blob.meta, raw_path, prep.blob.oid, mode_for_index);
        if icase_collisions.is_some_and(|map| map.is_auto_stage_loser(raw_path)) {
            continue;
        }
        if prep.blob.oid == idx_e.oid && mode_for_index == idx_e.mode {
            if stat_fields_differ(&entry, idx_e) {
                replacements.push(entry);
                modified.push(raw_path.to_vec());
            }
            continue;
        }
        needs_loose_write = true;
        modified.push(raw_path.to_vec());
        replacements.push(entry);
    }
    if needs_loose_write {
        if !*loose_dirs_ready {
            repo.odb().ensure_all_loose_prefix_dirs()?;
            *loose_dirs_ready = true;
        }
        for prep in &prepared {
            let raw_path = prep.blob.index_relpath.as_bytes();
            let Some(idx_e) = index.get(raw_path, 0) else {
                continue;
            };
            if prep.blob.oid == idx_e.oid
                && mode_policy.worktree_mode_matches(prep.blob.mode, idx_e.mode)
            {
                continue;
            }
            prep.payload.store(
                repo.odb(),
                &prep.blob.index_relpath,
                &prep.blob.oid,
                BULK_TRACKED_RESTAGE_LOOSE_WRITE,
            )?;
        }
    }
    Ok(replacements)
}

/// Whether `entry`'s cached stat fields differ from the index row it would replace.
fn stat_fields_differ(entry: &IndexEntry, idx_e: &IndexEntry) -> bool {
    entry.ctime_sec != idx_e.ctime_sec
        || entry.ctime_nsec != idx_e.ctime_nsec
        || entry.mtime_sec != idx_e.mtime_sec
        || entry.mtime_nsec != idx_e.mtime_nsec
        || entry.size != idx_e.size
        || entry.ino != idx_e.ino
        || entry.dev != idx_e.dev
        || entry.uid != idx_e.uid
        || entry.gid != idx_e.gid
}

fn refresh_gitlink(
    _repo: &Repository,
    index: &mut Index,
    raw_path: &[u8],
    abs_path: &Path,
    idx_e: &IndexEntry,
) -> Result<bool> {
    if let Some(oid) = read_submodule_head_oid(abs_path) {
        if index
            .entries
            .iter()
            .find(|e| e.path == *raw_path)
            .is_some_and(|e| e.oid == oid && e.mode == 0o160000)
        {
            return Ok(false);
        }
        stage_gitlink_from_stat(abs_path, raw_path, oid, index)?;
        return Ok(true);
    }
    let _ = idx_e;
    Ok(false)
}

fn stage_gitlink_from_stat(
    abs_path: &Path,
    raw_path: &[u8],
    oid: ObjectId,
    index: &mut Index,
) -> Result<()> {
    let mut entry = entry_from_stat(abs_path, raw_path, oid, MODE_GITLINK)?;
    entry.size = 0;
    index.add_or_replace(entry);
    Ok(())
}

fn stage_blob_from_worktree(
    repo: &Repository,
    index: &mut Index,
    abs_path: &Path,
    raw_path: &[u8],
    mode_override: Option<u32>,
    rules: &mut WorktreeRules,
) -> Result<()> {
    let meta = fs::symlink_metadata(abs_path)?;
    let oid = read_worktree_blob_oid(repo, abs_path, &meta, raw_path, rules)?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let mode = mode_override.unwrap_or_else(|| {
        #[cfg(unix)]
        {
            crate::index::normalize_mode(meta.mode())
        }
        #[cfg(not(unix))]
        {
            crate::index::MODE_REGULAR
        }
    });
    let entry = entry_from_stat(abs_path, raw_path, oid, mode)?;
    index.stage_file(entry);
    Ok(())
}

fn read_worktree_blob_oid(
    repo: &Repository,
    abs_path: &Path,
    meta: &fs::Metadata,
    raw_path: &[u8],
    rules: &mut WorktreeRules,
) -> Result<ObjectId> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(abs_path)?;
        let data = symlink_target_bytes(&target);
        return repo.odb().write(ObjectKind::Blob, &data);
    }
    if let Ok(rel_path) = std::str::from_utf8(raw_path) {
        let file_attrs = rules.file_attrs(rel_path, false);
        #[cfg(test)]
        repo.odb().hot_path_metrics().record_blob_content_read();
        return materialize_worktree_blob(
            repo.odb(),
            abs_path,
            meta,
            rules.conversion(),
            &file_attrs,
            rel_path,
            None,
            Some(rules.filter_process()),
        );
    }
    read_worktree_blob_oid_raw(repo, abs_path, meta)
}

fn read_worktree_blob_oid_raw(
    repo: &Repository,
    abs_path: &Path,
    meta: &fs::Metadata,
) -> Result<ObjectId> {
    let data = if meta.file_type().is_symlink() {
        let target = fs::read_link(abs_path)?;
        symlink_target_bytes(&target)
    } else {
        #[cfg(test)]
        repo.odb().hot_path_metrics().record_blob_content_read();
        fs::read(abs_path)?
    };
    repo.odb().write(ObjectKind::Blob, &data)
}

fn rel_path_has_symlink_parent(work_tree: &Path, rel_path: &str) -> bool {
    let components: Vec<&str> = rel_path.split('/').collect();
    if components.len() <= 1 {
        return false;
    }
    let mut prefix = String::new();
    for component in &components[..components.len() - 1] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let abs = work_tree.join(&prefix);
        if fs::symlink_metadata(&abs)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn path_has_symlink_parent_cached(
    work_tree: &Path,
    abs_path: &Path,
    cache: &mut HashMap<PathBuf, bool>,
) -> bool {
    let Some(parent) = abs_path.parent() else {
        return false;
    };
    dir_or_ancestor_is_symlink(work_tree, parent, cache)
}

fn dir_or_ancestor_is_symlink(
    work_tree: &Path,
    dir: &Path,
    cache: &mut HashMap<PathBuf, bool>,
) -> bool {
    if dir == work_tree || !dir.starts_with(work_tree) {
        return false;
    }
    if let Some(&answer) = cache.get(dir) {
        return answer;
    }
    let answer = match dir.parent() {
        Some(parent) if dir_or_ancestor_is_symlink(work_tree, parent, cache) => true,
        _ => fs::symlink_metadata(dir)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
    };
    cache.insert(dir.to_path_buf(), answer);
    answer
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use filetime::{set_file_mtime, FileTime};
    use std::fs;
    use std::io::Write;

    use crate::diff::{entry_is_racy, mode_from_metadata, stat_matches};
    use crate::index::{
        entry_from_metadata, index_file_mtime, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK,
        MODE_REGULAR,
    };
    use crate::objects::{ObjectId, ObjectKind};
    use crate::odb::Odb;
    use crate::repo::{init_repository, Repository};
    use tempfile::TempDir;

    use super::*;

    const INDEX_MTIME: (u32, u32) = (1_700_000_000, 123_456_789);

    fn init_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        (dir, repo)
    }

    fn jobs_with_sizes(sizes: &[u64]) -> Vec<DeferredRestageJob> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| DeferredRestageJob {
                raw_path: format!("f{i}").into_bytes(),
                size,
            })
            .collect()
    }

    #[test]
    fn size_changed_stream_hashes_only_during_storage() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let data = vec![b'x'; 2 * 1024 * 1024];
        write_and_index(&repo, b"large", &data);
        fs::OpenOptions::new()
            .append(true)
            .open(wt.join("large"))
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        let mut index = repo.load_index().unwrap();
        let before = repo.odb().hot_path_metrics().stream_hash_calls();
        let writes_before = repo.odb().hot_path_metrics().stream_write_calls();
        stage_tracked_modifications_in_index(&repo, wt, &repo.index_path(), &mut index).unwrap();
        assert_eq!(repo.odb().hot_path_metrics().stream_hash_calls(), before);
        assert_eq!(
            repo.odb().hot_path_metrics().stream_write_calls(),
            writes_before + 1
        );
        let stored = repo.odb().read(&index.entries[0].oid).unwrap();
        assert_eq!(stored.data.len(), data.len() + 7);
        assert!(stored.data.ends_with(b"changed"));
    }

    #[test]
    fn stale_stat_size_does_not_materialize_unchanged_stream() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let data = vec![b'x'; 2 * 1024 * 1024];
        write_and_index(&repo, b"large", &data);
        let mut index = repo.load_index().unwrap();
        let oid = index.entries[0].oid;
        index.entries[0].size = 0;
        let before = repo.odb().hot_path_metrics().stream_hash_calls();
        let writes_before = repo.odb().hot_path_metrics().stream_write_calls();
        stage_tracked_modifications_in_index(&repo, wt, &repo.index_path(), &mut index).unwrap();
        assert_eq!(
            repo.odb().hot_path_metrics().stream_hash_calls(),
            before + 1
        );
        assert_eq!(
            repo.odb().hot_path_metrics().stream_write_calls(),
            writes_before
        );
        assert_eq!(index.entries[0].oid, oid);
        assert_eq!(index.entries[0].size as usize, data.len());
    }

    #[test]
    fn size_changed_stream_can_repair_a_missing_index_object() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        write_and_index(&repo, b"large", b"old");
        let mut index = repo.load_index().unwrap();
        fs::remove_file(repo.odb().object_path(&index.entries[0].oid)).unwrap();
        let data = vec![b'x'; 2 * 1024 * 1024];
        fs::write(wt.join("large"), &data).unwrap();
        stage_tracked_modifications_in_index(&repo, wt, &repo.index_path(), &mut index).unwrap();
        assert_eq!(repo.odb().read(&index.entries[0].oid).unwrap().data, data);
    }

    #[test]
    fn changed_stream_does_not_fetch_a_missing_previous_blob() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        struct FetchCounter(AtomicUsize);
        impl crate::promisor_fetch::PromisorFetcher for FetchCounter {
            fn fetch_missing(&self, _: &Repository, _: &[ObjectId]) -> Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(Error::Message("unexpected fetch".into()))
            }
        }
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        write_and_index(&repo, b"large", b"old");
        let mut index = repo.load_index().unwrap();
        fs::remove_file(repo.odb().object_path(&index.entries[0].oid)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(repo.git_dir().join("config"))
            .unwrap()
            .write_all(b"[remote \"origin\"]\n promisor = true\n partialclonefilter = blob:none\n")
            .unwrap();
        let fetcher = Arc::new(FetchCounter(AtomicUsize::new(0)));
        let opts = crate::environment::RepositoryOptions::empty()
            .with_work_tree(wt)
            .with_promisor_fetcher(Some(fetcher.clone()));
        let repo = Repository::open(repo.git_dir(), &opts).unwrap();
        fs::write(wt.join("large"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
        stage_tracked_modifications_in_index(&repo, wt, &repo.index_path(), &mut index).unwrap();
        assert_eq!(fetcher.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            repo.odb().read(&index.entries[0].oid).unwrap().data.len(),
            2 * 1024 * 1024
        );
    }

    #[test]
    fn restage_chunks_bound_job_count_and_bytes() {
        assert!(restage_chunks(&[]).is_empty());

        let many = jobs_with_sizes(&vec![10; RESTAGE_CHUNK_MAX_JOBS * 2 + 5]);
        let lens: Vec<usize> = restage_chunks(&many).iter().map(|c| c.len()).collect();
        assert_eq!(
            lens,
            [RESTAGE_CHUNK_MAX_JOBS, RESTAGE_CHUNK_MAX_JOBS, 5],
            "job count bound"
        );

        let half = RESTAGE_CHUNK_MAX_BYTES / 2;
        let big = jobs_with_sizes(&[half, half, 1, RESTAGE_CHUNK_MAX_BYTES * 3, 1]);
        let lens: Vec<usize> = restage_chunks(&big).iter().map(|c| c.len()).collect();
        assert_eq!(lens, [2, 1, 1, 1], "byte bound; an oversized file is alone");

        let total: usize = restage_chunks(&many).iter().map(|c| c.len()).sum();
        assert_eq!(total, many.len());
    }

    #[test]
    fn stage_tracked_after_deleting_first_of_three_paths() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"a.txt", b"a\n");
        write_and_index(&repo, b"b.txt", b"b\n");
        write_and_index(&repo, b"c.txt", b"c\n");
        finalize_index_for_stat_trust(&repo);

        let wt = repo.work_tree().unwrap();
        fs::remove_file(worktree_path_from_index_rel(wt, b"a.txt")).unwrap();

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.removed, vec![b"a.txt".to_vec()]);
        assert!(summary.modified.is_empty());
        assert!(!repo
            .load_index()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.path == b"a.txt"));
    }

    #[test]
    fn staging_scan_stats_each_tracked_path_once() {
        use crate::worktree_scan::{reset_worktree_metadata_probe, worktree_metadata_probe_count};

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        const FILE_COUNT: usize = 2000;
        for i in 0..FILE_COUNT {
            let rel = format!("d{:04}/f{:05}.txt", i / 100, i);
            write_and_index(&repo, rel.as_bytes(), b"x\n");
        }
        let _probe = reset_worktree_metadata_probe(wt);
        let index_path = repo.index_path_for_env().unwrap();
        let mut index = repo.load_index().unwrap();
        stage_tracked_modifications_in_index(&repo, wt, &index_path, &mut index).unwrap();
        let tracked = index.entries.len();
        let dir_count = (FILE_COUNT + 99) / 100;
        let calls = worktree_metadata_probe_count();
        assert!(
            calls <= tracked + dir_count + 5,
            "staging scan metadata calls {calls} exceeded budget {} (tracked {tracked}, dirs {dir_count})",
            tracked + dir_count + 5
        );
    }

    fn pin_mtime(path: &Path, sec: u32, nsec: u32) {
        set_file_mtime(path, FileTime::from_unix_time(i64::from(sec), nsec)).unwrap();
    }

    fn write_and_index(repo: &Repository, rel: &[u8], content: &[u8]) {
        let wt = repo.work_tree().unwrap();
        let abs = worktree_path_from_index_rel(wt, rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, content).unwrap();
        let oid = repo.odb().write(ObjectKind::Blob, content).unwrap();
        let mut index = repo.load_index().unwrap();
        let meta = fs::symlink_metadata(&abs).unwrap();
        let mode = mode_from_metadata(&meta);
        let entry = entry_from_stat(&abs, rel, oid, mode).unwrap();
        index.add_or_replace(entry);
        repo.write_index(&mut index).unwrap();
    }

    fn finalize_index_for_stat_trust(repo: &Repository) {
        let wt = repo.work_tree().unwrap();
        let mut index = repo.load_index().unwrap();
        for entry in &mut index.entries {
            if entry.stage() != 0 {
                continue;
            }
            let abs = worktree_path_from_index_rel(wt, &entry.path);
            if let Ok(fresh) = entry_from_stat(&abs, &entry.path, entry.oid, entry.mode) {
                *entry = fresh;
            }
        }
        repo.write_index(&mut index).unwrap();
        let index_path = repo.index_path();
        let index = repo.load_index().unwrap();
        let (max_sec, max_nsec) = index
            .entries
            .iter()
            .map(|e| (e.mtime_sec, e.mtime_nsec))
            .max()
            .unwrap_or((0, 0));
        pin_mtime(&index_path, max_sec.saturating_add(2), max_nsec);
    }

    #[test]
    fn stages_modified_tracked_file() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"a.txt", b"one");
        write_and_index(&repo, b"b.txt", b"two");
        finalize_index_for_stat_trust(&repo);
        fs::write(
            worktree_path_from_index_rel(repo.work_tree().unwrap(), b"b.txt"),
            b"two-changed",
        )
        .unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree().unwrap()).unwrap();
        assert_eq!(summary.modified, vec![b"b.txt".to_vec()]);
        assert!(summary.removed.is_empty());

        let index = repo.load_index().unwrap();
        let b = index.entries.iter().find(|e| e.path == b"b.txt").unwrap();
        let expected = repo.odb().write(ObjectKind::Blob, b"two-changed").unwrap();
        assert_eq!(b.oid, expected);
    }

    #[test]
    fn removes_deleted_tracked_file() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"gone.txt", b"x");
        fs::remove_file(worktree_path_from_index_rel(
            repo.work_tree().unwrap(),
            b"gone.txt",
        ))
        .unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree().unwrap()).unwrap();
        assert_eq!(summary.removed, vec![b"gone.txt".to_vec()]);
        assert!(repo.load_index().unwrap().entries.is_empty());
    }

    #[test]
    fn unchanged_tracked_file_is_not_rehashed() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"clean.txt", b"same");
        finalize_index_for_stat_trust(&repo);

        let objects_before = count_loose_objects(&repo);
        let summary = stage_tracked_modifications(&repo, repo.work_tree().unwrap()).unwrap();
        let objects_after = count_loose_objects(&repo);

        assert!(summary.modified.is_empty());
        assert!(summary.removed.is_empty());
        assert_eq!(objects_before, objects_after);
    }

    fn count_loose_objects(repo: &Repository) -> usize {
        repo.odb().count_loose_objects().unwrap_or(0)
    }

    #[test]
    fn racy_same_size_modification_is_detected() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let content_a = b"12345";
        let content_b = b"54321";
        assert_eq!(content_a.len(), content_b.len());

        write_and_index(&repo, b"racy.txt", content_a);
        let file_path = worktree_path_from_index_rel(wt, b"racy.txt");
        fs::write(&file_path, content_b).unwrap();
        pin_mtime(&file_path, INDEX_MTIME.0, INDEX_MTIME.1);

        let meta = fs::symlink_metadata(&file_path).unwrap();
        let stale_oid = repo.odb().hash(ObjectKind::Blob, content_a).expect("hash");
        let mut index = repo.load_index().unwrap();
        let entry = entry_from_metadata(&meta, b"racy.txt", stale_oid, MODE_REGULAR);
        assert!(stat_matches(&entry, &meta));
        assert!(entry_is_racy(&entry, Some(INDEX_MTIME)));

        index.entries.clear();
        index.add_or_replace(entry);
        let index_path = repo.index_path();
        index.write(&index_path).unwrap();
        pin_mtime(&index_path, INDEX_MTIME.0, INDEX_MTIME.1);

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.modified, vec![b"racy.txt".to_vec()]);
        let expected = repo.odb().write(ObjectKind::Blob, content_b).unwrap();
        let loaded = repo.load_index().unwrap();
        let e = loaded
            .entries
            .iter()
            .find(|e| e.path == b"racy.txt")
            .unwrap();
        assert_eq!(e.oid, expected);
    }

    #[test]
    fn index_write_smudge_then_stage_detects_stale_oid() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let content_a = b"aaaaa";
        let content_b = b"bbbbb";
        assert_eq!(content_a.len(), content_b.len());

        let file_path = worktree_path_from_index_rel(wt, b"f.txt");
        fs::write(&file_path, content_b).unwrap();
        pin_mtime(&file_path, INDEX_MTIME.0, INDEX_MTIME.1);
        let meta = fs::symlink_metadata(&file_path).unwrap();
        let stale_oid = repo.odb().hash(ObjectKind::Blob, content_a).expect("hash");

        let mut index = Index::new();
        let entry = entry_from_metadata(&meta, b"f.txt", stale_oid, MODE_REGULAR);
        assert!(stat_matches(&entry, &meta));
        assert!(entry_is_racy(&entry, Some(INDEX_MTIME)));
        index.add_or_replace(entry);

        let index_path = repo.index_path();
        index.write(&index_path).unwrap();
        pin_mtime(&index_path, INDEX_MTIME.0, INDEX_MTIME.1);

        let later = (INDEX_MTIME.0 + 10, INDEX_MTIME.1);
        pin_mtime(&index_path, later.0, later.1);

        let mut index = repo.load_index().unwrap();
        assert!(
            stage_tracked_modifications_in_index(&repo, wt, &index_path, &mut index)
                .unwrap()
                .modified
                .is_empty(),
            "stale oid trusted when index mtime advanced without smudging"
        );

        pin_mtime(&index_path, INDEX_MTIME.0, INDEX_MTIME.1);
        repo.write_index(&mut index).unwrap();
        let index = repo.load_index().unwrap();
        assert_eq!(
            index.entries[0].size, 0,
            "write_index should smudge racily-clean stale entries"
        );

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.modified, vec![b"f.txt".to_vec()]);
    }

    #[test]
    fn index_file_mtime_available_on_all_targets() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("index");
        fs::write(&path, b"dummy").unwrap();
        pin_mtime(&path, 1_234, 567);
        let m = index_file_mtime(&path).expect("mtime");
        assert_eq!(m.0, 1_234);
        assert_eq!(m.1, 567);
    }

    #[test]
    #[cfg(unix)]
    fn mode_only_change_is_staged() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"run.sh", b"#!/bin/sh\n");
        finalize_index_for_stat_trust(&repo);
        let path = worktree_path_from_index_rel(repo.work_tree().unwrap(), b"run.sh");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o100755);
        fs::set_permissions(&path, perms).unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree().unwrap()).unwrap();
        assert_eq!(summary.modified, vec![b"run.sh".to_vec()]);
        let index = repo.load_index().unwrap();
        let e = index.entries.iter().find(|e| e.path == b"run.sh").unwrap();
        assert_eq!(e.mode, MODE_EXECUTABLE);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_under_symlinked_parent() {
        let dir = TempDir::new().unwrap();
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        let wt = repo.work_tree().unwrap();
        fs::create_dir(wt.join("realdir")).unwrap();
        fs::write(wt.join("realdir/file"), b"data").unwrap();
        std::os::unix::fs::symlink("realdir", wt.join("linkdir")).unwrap();

        write_and_index(&repo, b"linkdir/file", b"data");
        finalize_index_for_stat_trust(&repo);

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert!(summary.removed.contains(&b"linkdir/file".to_vec()));
        assert!(!repo
            .load_index()
            .unwrap()
            .entries
            .iter()
            .any(|e| e.path == b"linkdir/file"));
    }

    #[test]
    fn untracked_files_are_ignored() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"tracked.txt", b"x");
        finalize_index_for_stat_trust(&repo);
        fs::write(
            worktree_path_from_index_rel(repo.work_tree().unwrap(), b"new.txt"),
            b"new",
        )
        .unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree().unwrap()).unwrap();
        assert!(summary.modified.is_empty());
        assert!(summary.removed.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_parent_does_not_stage_deletion() {
        use std::io::ErrorKind;
        use std::os::unix::fs::PermissionsExt;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let locked = wt.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("f"), b"secret").unwrap();
        write_and_index(&repo, b"locked/f", b"secret");
        finalize_index_for_stat_trust(&repo);

        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms).unwrap();

        let err = stage_tracked_modifications(&repo, wt).unwrap_err();
        let Error::Io(io_err) = err else {
            panic!("expected I/O error, got {err:?}");
        };
        assert_eq!(io_err.kind(), ErrorKind::PermissionDenied);

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        let index = repo.load_index().unwrap();
        assert!(index.entries.iter().any(|e| e.path == b"locked/f"));
    }

    #[test]
    fn precomposed_index_path_resolves_decomposed_worktree_file() {
        let (_dir, repo) = init_repo();
        fs::OpenOptions::new()
            .append(true)
            .open(repo.git_dir().join("config"))
            .unwrap()
            .write_all(b"\n[core]\n\tprecomposeunicode = true\n")
            .unwrap();
        repo.reload_config().unwrap();

        let wt = repo.work_tree().unwrap();
        let nfd = "cafe\u{0301}.txt";
        let nfc = "caf\u{00e9}.txt";
        let abs = wt.join(nfd);
        fs::write(&abs, b"one").unwrap();
        let oid = repo.odb().write(ObjectKind::Blob, b"one").unwrap();
        let meta = fs::symlink_metadata(&abs).unwrap();
        let mut index = repo.load_index().unwrap();
        index.add_or_replace(
            entry_from_stat(&abs, nfc.as_bytes(), oid, mode_from_metadata(&meta)).unwrap(),
        );
        repo.write_index(&mut index).unwrap();

        let unchanged = stage_tracked_modifications(&repo, wt).unwrap();
        assert!(unchanged.modified.is_empty());
        assert!(unchanged.removed.is_empty());

        fs::write(&abs, b"changed").unwrap();
        let changed = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(changed.modified, vec![nfc.as_bytes().to_vec()]);
        assert!(changed.removed.is_empty());
        let expected = repo.odb().write(ObjectKind::Blob, b"changed").unwrap();
        assert_eq!(repo.load_index().unwrap().entries[0].oid, expected);
    }

    #[test]
    fn filemode_false_skips_executable_bit_only_refresh() {
        let dir = TempDir::new().unwrap();
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        fs::write(
            repo.git_dir().join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n",
        )
        .unwrap();
        repo.reload_config().unwrap();
        fs::write(dir.path().join("run.sh"), b"x\n").unwrap();
        let git_dir = repo.git_dir().to_path_buf();
        let wt = repo.work_tree().unwrap();
        let odb = Odb::new(&git_dir.join("objects"));
        let oid = odb.write(ObjectKind::Blob, b"x\n").unwrap();
        let abs = wt.join("run.sh");
        let meta = fs::symlink_metadata(&abs).unwrap();
        let mut entry = entry_from_stat(&abs, b"run.sh", oid, mode_from_metadata(&meta)).unwrap();
        entry.mode = MODE_EXECUTABLE;
        let mut index = Index::new();
        index.add_or_replace(entry);
        repo.write_index(&mut index).unwrap();
        finalize_index_for_stat_trust(&repo);

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert!(
            summary.modified.is_empty(),
            "core.filemode=false must not stage mode-only executable mismatch: {summary:?}"
        );
        let loaded = repo.load_index().unwrap();
        let e = loaded.entries.iter().find(|e| e.path == b"run.sh").unwrap();
        assert_eq!(e.mode, MODE_EXECUTABLE);
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_tracked_path_is_staged_not_removed() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let rel = b"\xff";
        let abs = wt.join(OsStr::from_bytes(rel));
        fs::write(&abs, b"v1").unwrap();
        write_and_index(&repo, rel, b"v1");
        finalize_index_for_stat_trust(&repo);
        fs::write(&abs, b"v2-longer").unwrap();

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.modified, vec![rel.to_vec()]);
        assert!(summary.removed.is_empty());
        let index = repo.load_index().unwrap();
        assert!(index.entries.iter().any(|e| e.path == rel));
    }

    #[test]
    fn pathless_add_promotes_intent_to_add_entry() {
        use std::process::Command;

        if !Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return;
        }
        let (dir, repo) = init_repo();
        let root = dir.path();
        grit_test_support::git(root, &["config", "user.email", "t@example.com"]);
        grit_test_support::git(root, &["config", "user.name", "Test"]);
        fs::write(root.join("f.txt"), b"hello\n").unwrap();
        grit_test_support::git(root, &["add", "-N", "f.txt"]);

        let empty = crate::diff::empty_blob_oid();
        let before = repo.load_index().unwrap();
        let ie = before.get(b"f.txt", 0).expect("ita entry");
        assert!(ie.intent_to_add());
        assert_eq!(ie.oid, empty);

        use crate::porcelain::add::{stage, StageOptions};
        use crate::progress::NullProgress;
        let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();
        assert_eq!(
            outcome.modified.len(),
            1,
            "pathless add must promote -N entry"
        );

        let after = repo.load_index().unwrap();
        let ie = after.get(b"f.txt", 0).expect("staged f.txt");
        assert!(!ie.intent_to_add(), "full add clears intent-to-add");
        assert_ne!(ie.oid, empty, "index must store real content blob");
    }

    #[test]
    #[cfg(unix)]
    fn defer_untracked_flag_cleared_after_io_error() {
        let thresholds = IndexStagingThresholds {
            large_index_restage: 4,
            parallel_tracked_restage: 4,
        };

        let dir = TempDir::new().unwrap();
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .unwrap();
        let wt = repo.work_tree().unwrap();
        for i in 0..6 {
            let d = wt.join(format!("d{i}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("f.txt"), b"x\n").unwrap();
        }
        write_and_index(&repo, b"d0/f.txt", b"old\n");
        write_and_index(&repo, b"d1/f.txt", b"y\n");
        write_and_index(&repo, b"d2/f.txt", b"y\n");
        write_and_index(&repo, b"d3/f.txt", b"y\n");
        write_and_index(&repo, b"d4/f.txt", b"y\n");
        write_and_index(&repo, b"d5/f.txt", b"y\n");
        finalize_index_for_stat_trust(&repo);

        let blocked = wt.join("d0/f.txt");
        fs::write(&blocked, b"changed\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

        let index_path = repo.index_path_for_env().unwrap();
        let mut index = repo.load_index().unwrap();
        index.defer_untracked_cache_invalidation = false;
        let err = stage_tracked_modifications_in_index_with_thresholds(
            &repo,
            wt,
            &index_path,
            &mut index,
            thresholds,
        )
        .expect_err("unreadable blob should fail restage");
        assert!(
            !index.defer_untracked_cache_invalidation,
            "defer flag must be cleared after error: {err}"
        );

        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn conflict_index_entry(path: &str, stage: u8, oid: ObjectId, mode: u32) -> IndexEntry {
        let path_bytes = path.as_bytes().to_vec();
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
            flags: (path_bytes.len().min(0xFFF) as u16) | ((stage as u16) << 12),
            flags_extended: None,
            path: path_bytes,
            base_index_pos: 0,
        }
    }

    fn open_nested_repo(nested_root: &Path) -> Repository {
        use crate::config::environment::RepositoryOptions;

        let git = nested_root.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
        Repository::discover(Some(nested_root), &RepositoryOptions::empty()).unwrap()
    }

    fn commit_in_nested_repo(
        repo: &Repository,
        rel_file: &str,
        content: &[u8],
        msg: &str,
    ) -> ObjectId {
        let wt = repo.work_tree().unwrap();
        let abs = wt.join(rel_file);
        if let Some(p) = abs.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&abs, content).unwrap();
        let blob = repo.odb().write(ObjectKind::Blob, content).unwrap();
        let tree_body = format!("100644 {blob}\t{rel_file}\n");
        let tree = repo
            .odb()
            .write(ObjectKind::Tree, tree_body.as_bytes())
            .unwrap();
        let commit_body =
            format!("tree {tree}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\n{msg}\n");
        repo.odb()
            .write(ObjectKind::Commit, commit_body.as_bytes())
            .unwrap()
    }

    fn set_branch_head(repo: &Repository, oid: ObjectId) {
        let head_path = repo.git_dir().join("refs/heads/main");
        fs::write(head_path, format!("{oid}\n")).unwrap();
    }

    #[test]
    fn unmerged_gitlink_auto_stage_reads_submodule_head_not_directory() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let sub = wt.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let sub_repo = open_nested_repo(&sub);
        let base_oid = commit_in_nested_repo(&sub_repo, "inside.txt", b"base\n", "base");
        let ours_oid = commit_in_nested_repo(&sub_repo, "inside.txt", b"ours\n", "ours");
        let theirs_oid = commit_in_nested_repo(&sub_repo, "inside.txt", b"theirs\n", "theirs");
        set_branch_head(&sub_repo, theirs_oid);

        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("sub", 1, base_oid, MODE_GITLINK));
        index
            .entries
            .push(conflict_index_entry("sub", 2, ours_oid, MODE_GITLINK));
        index
            .entries
            .push(conflict_index_entry("sub", 3, theirs_oid, MODE_GITLINK));
        repo.write_index(&mut index).unwrap();

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.modified, vec![b"sub".to_vec()]);

        let index = repo.load_index().unwrap();
        assert!(
            !index.has_unmerged_entries(),
            "checked-out submodule should collapse unmerged gitlink stages"
        );
        let staged = index.get(b"sub", 0).expect("stage-0 gitlink");
        assert_eq!(staged.mode, MODE_GITLINK);
        assert_eq!(staged.oid, theirs_oid);
    }

    #[test]
    fn gitlink_replaced_by_regular_file_stages_blob() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        fs::write(
            repo.git_dir().join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n",
        )
        .unwrap();
        repo.reload_config().unwrap();
        let gitlink_oid = repo.odb().write(ObjectKind::Commit, b"tree 0000000000000000000000000000000000000000\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\nsub\n").unwrap();
        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("sub", 0, gitlink_oid, MODE_GITLINK));
        repo.write_index(&mut index).unwrap();

        fs::write(wt.join("sub"), b"plain file\n").unwrap();

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.modified, vec![b"sub".to_vec()]);

        let index = repo.load_index().unwrap();
        let staged = index.get(b"sub", 0).expect("stage-0 blob");
        assert_eq!(
            staged.mode, MODE_REGULAR,
            "core.filemode=false must not preserve gitlink mode on type change"
        );
        assert_ne!(staged.oid, gitlink_oid);
    }

    #[test]
    #[cfg(unix)]
    fn unmerged_symlink_auto_stage_reads_link_target_not_file_contents() {
        use std::os::unix::fs::symlink;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().unwrap();
        let target = "does-not-exist";
        let link_path = wt.join("f");
        let base = repo.odb().write(ObjectKind::Blob, b"base\n").unwrap();
        let ours = repo.odb().write(ObjectKind::Blob, b"ours\n").unwrap();
        let theirs = repo.odb().write(ObjectKind::Blob, b"theirs\n").unwrap();

        let mut index = Index::new();
        index
            .entries
            .push(conflict_index_entry("f", 1, base, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f", 2, ours, MODE_REGULAR));
        index
            .entries
            .push(conflict_index_entry("f", 3, theirs, MODE_REGULAR));
        repo.write_index(&mut index).unwrap();

        symlink(target, &link_path).unwrap();

        let summary = stage_tracked_modifications(&repo, wt).unwrap();
        assert_eq!(summary.modified, vec![b"f".to_vec()]);

        let index = repo.load_index().unwrap();
        assert!(
            !index.has_unmerged_entries(),
            "dangling symlink resolution should collapse unmerged stages"
        );
        let staged = index.get(b"f", 0).expect("stage-0 symlink");
        assert!(
            staged.mode & 0o170000 == 0o120000,
            "expected symlink mode, got {:o}",
            staged.mode
        );
        assert_eq!(
            repo.odb().read(&staged.oid).unwrap().data,
            target.as_bytes()
        );
    }
}
