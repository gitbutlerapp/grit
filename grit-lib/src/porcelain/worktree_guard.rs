//! Lightweight worktree cleanliness checks without a full [`status`](super::status::status) pass.
//!
//! Pick, merge, and branch switch only need to know whether staged or unstaged changes exist
//! (and switch additionally needs untracked paths plus a tree-to-tree diff). This module loads
//! the index once, refreshes stat data when needed, and computes those slices without rename
//! detection, ahead/behind, stash counts, or in-progress operation state.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::ConfigSet;
use crate::diff::format_mode;
use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, entry_is_racy_with_policy,
    hash_worktree_file, index_mtime_for_diff, DiffEntry, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::StatMatchPolicy;
use crate::index::{index_file_mtime, Index, IndexEntry};
use crate::objects::{parse_commit, ObjectId};
use crate::parallel::{try_par_map, ParallelHashError, ParallelOp};
use crate::porcelain::checkout::tree_changes_touch_gitattributes;
use crate::porcelain::status::{collect_untracked_and_ignored_with_cache, IgnoredMode};
use crate::repo::Repository;
use crate::state::resolve_head;
use crate::worktree_rules::WorktreeRules;

/// Loaded index and HEAD tree for worktree mutation guards.
pub struct WorktreeSnapshot {
    /// Index with sparse-directory placeholders expanded.
    pub index: Index,
    /// Tree OID of the current HEAD commit, if any.
    pub head_tree: Option<ObjectId>,
}

/// Load the index and resolve HEAD's tree OID (no worktree scan).
///
/// # Errors
///
/// Returns errors from index I/O, ODB reads, or when the repository has no work tree.
pub fn load_worktree_snapshot(repo: &Repository) -> Result<WorktreeSnapshot> {
    let _work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    let head = resolve_head(repo.git_dir())?;
    let head_tree = head_tree_from_head_commit(repo, head.oid())?;
    let index = load_worktree_index(repo)?;
    Ok(WorktreeSnapshot { index, head_tree })
}

/// Load the index for worktree guards without resolving `HEAD`.
///
/// The cache-tree extension is kept: the staged-change check walks it instead of flattening
/// the whole `HEAD` tree, and index writes after a checkout carry it forward.
pub(crate) fn load_worktree_index(repo: &Repository) -> Result<Index> {
    let _work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path_for_env()?;
    let mut index = repo.load_index_at(&index_path)?;
    if index.has_sparse_directory_placeholders() {
        let protect = crate::path_validate::ProtectOptions::from_config(&*repo.config()?);
        index.expand_sparse_directory_placeholders(repo.odb(), protect)?;
    }
    Ok(index)
}

fn head_tree_from_head_commit(
    repo: &Repository,
    head_oid: Option<&ObjectId>,
) -> Result<Option<ObjectId>> {
    let Some(oid) = head_oid else {
        return Ok(None);
    };
    let obj = repo.odb().read(oid)?;
    Ok(Some(parse_commit(&obj.data)?.tree))
}

/// Build a snapshot when `HEAD`'s tree is already known (avoids re-resolving `HEAD`).
pub(crate) fn worktree_snapshot_with_head_tree(
    repo: &Repository,
    head_tree: Option<ObjectId>,
) -> Result<WorktreeSnapshot> {
    let index = load_worktree_index(repo)?;
    Ok(WorktreeSnapshot { index, head_tree })
}

/// Staged and unstaged diffs for the current worktree (no rename detection), with stat refresh
/// in the same worktree pass as the unstaged diff.
pub(crate) fn local_change_diffs(
    repo: &Repository,
    index: &mut Index,
    head_tree: Option<ObjectId>,
) -> Result<(Vec<DiffEntry>, Vec<DiffEntry>, bool)> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path_for_env()?;
    let index_mtime = index_file_mtime(&index_path);
    let config = ConfigSet::load(repo.environment(), Some(repo.git_dir()), true)
        .ok()
        .map(Arc::new);
    let worktree_rules = match config.as_ref() {
        Some(cfg) => Some(Arc::new(Mutex::new(WorktreeRules::from_parts(
            repo,
            index,
            cfg.clone(),
        )?))),
        None => None,
    };

    let staged = diff_index_to_tree(repo.odb(), index, head_tree.as_ref(), false)?;
    let (unstaged, index_changed) = diff_index_to_worktree_with_options(
        repo.odb(),
        index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            index_mtime: crate::diff::index_mtime_for_diff(index, index_mtime),
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            refresh_index_stat_in_pass: false,
            refresh_fsmonitor: false,
            config,
            worktree_rules,
            command_runner: Some(repo.command_runner()),
            ..Default::default()
        },
    )?;
    Ok((staged, unstaged, index_changed))
}

/// Like [`local_change_diffs`], but only considers paths touched by `tree_changes`.
///
/// Falls back to a full index/worktree scan when the index has unmerged entries.
pub(crate) fn local_change_diffs_for_tree_changes(
    repo: &Repository,
    index: &mut Index,
    head_tree: Option<ObjectId>,
    tree_changes: &[DiffEntry],
    skip_worktree_stat_paths: Option<&BTreeSet<String>>,
) -> Result<(Vec<DiffEntry>, Vec<DiffEntry>, bool)> {
    let _ = head_tree;
    if index.entries.iter().any(|e| e.stage() != 0) {
        return local_change_diffs(repo, index, head_tree);
    }
    let paths = paths_touched_by_tree_changes(tree_changes);
    if paths.is_empty() {
        return Ok((Vec::new(), Vec::new(), false));
    }
    let path_count = paths.len();
    let path_filter = Arc::new(paths);
    let head_by_path = head_tree_sides_from_changes(tree_changes);
    let staged = staged_diff_for_tree_switch_paths(index, &head_by_path, path_filter.as_ref())?;

    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path_for_env()?;
    let index_mtime = index_mtime_for_diff(index, index_file_mtime(&index_path));
    let stat_policy = StatMatchPolicy::from_config(repo.config().ok().as_deref());
    let added_tree_paths: HashSet<String> = tree_changes
        .iter()
        .filter(|change| change.status == DiffStatus::Added)
        .filter_map(|change| change.new_path.clone())
        .collect();
    let scheduled_delete_paths: HashSet<String> = tree_changes
        .iter()
        .filter(|change| change.status == DiffStatus::Deleted)
        .filter_map(|change| change.old_path.clone())
        .collect();
    let paths_for_stat: BTreeSet<String> = {
        let base: BTreeSet<String> = skip_worktree_stat_paths.map_or_else(
            || path_filter.as_ref().clone(),
            |skip| {
                path_filter
                    .iter()
                    .filter(|path| !skip.contains(*path))
                    .cloned()
                    .collect()
            },
        );
        base.into_iter()
            .filter(|path| {
                index.get(path.as_bytes(), 0).is_some() || added_tree_paths.contains(path)
            })
            .filter(|path| {
                !delete_path_trusted_without_worktree_stat(
                    work_tree,
                    index,
                    &head_by_path,
                    &scheduled_delete_paths,
                    path,
                    index_mtime,
                    stat_policy,
                )
            })
            .filter(|path| !added_tree_path_missing_on_disk(work_tree, &added_tree_paths, path))
            .collect()
    };
    if staged.is_empty()
        && !touched_paths_include_gitlinks(index, path_filter.as_ref())
        && (paths_for_stat.is_empty()
            || worktree_clean_for_switch_paths(
                repo,
                index,
                &paths_for_stat,
                index_mtime,
                stat_policy,
            )?)
    {
        return Ok((staged, Vec::new(), false));
    }

    let config = repo.config().ok();
    let worktree_rules = match config.as_ref() {
        Some(cfg) => {
            let rules = if tree_changes_touch_gitattributes(tree_changes) {
                WorktreeRules::from_parts(repo, index, Arc::clone(cfg))?
            } else {
                WorktreeRules::from_parts_index_attributes_only(repo, index, Arc::clone(cfg))?
            };
            Some(Arc::new(Mutex::new(rules)))
        }
        None => None,
    };
    let stat_parallel_threads = (path_count >= 32).then(|| {
        repo.thread_policy()
            .resolve(
                crate::parallel::ParallelOp::IndexRefresh,
                config.as_deref().unwrap_or(&ConfigSet::new()),
            )
            .threads
            .get()
    });
    let (mut unstaged, index_changed) = diff_index_to_worktree_with_options(
        repo.odb(),
        index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            index_mtime,
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            refresh_index_stat_in_pass: false,
            refresh_fsmonitor: false,
            for_status: false,
            stat_parallel_threads,
            config,
            worktree_rules,
            command_runner: Some(repo.command_runner()),
            restrict_to_paths: Some(path_filter),
            ..Default::default()
        },
    )?;
    unstaged.retain(|entry| {
        let Some(path) = entry.old_path.as_deref().or(entry.new_path.as_deref()) else {
            return true;
        };
        !delete_path_trusted_without_worktree_stat(
            work_tree,
            index,
            &head_by_path,
            &scheduled_delete_paths,
            path,
            index_mtime,
            stat_policy,
        )
    });
    Ok((staged, unstaged, index_changed))
}

/// True when a touched path is a gitlink in the index (requires full submodule checks, not stat-only).
fn touched_paths_include_gitlinks(index: &Index, paths: &BTreeSet<String>) -> bool {
    use crate::index::MODE_GITLINK;
    paths.iter().any(|path| {
        index
            .get(path.as_bytes(), 0)
            .is_some_and(|ie| ie.stage() == 0 && ie.mode == MODE_GITLINK)
    })
}

/// Whether indexed blobs under `paths` match the worktree for switch (stat-only, plus content when racy).
fn worktree_clean_for_switch_paths(
    repo: &Repository,
    index: &Index,
    paths: &BTreeSet<String>,
    index_mtime: Option<(u32, u32)>,
    stat_policy: StatMatchPolicy,
) -> Result<bool> {
    use crate::crlf;
    use crate::diff::index_entry_worktree_abs;
    use crate::diff::stat_matches_with_policy;
    use crate::index::MODE_GITLINK;
    use crate::parallel::ParallelOp;
    use crate::worktree_scan::{
        collect_blob_lookup_batches, for_each_blob_by_directory,
        group_blob_entries_by_dir_for_paths, visit_blob_lookup_batches, BlobDiskLookup,
        BlobScanSparseMode, WorktreeBlobScanOptions,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let config = repo.config().ok();
    let precompose_unicode = config
        .as_ref()
        .and_then(|c| c.get_bool("core.precomposeunicode").and_then(|r| r.ok()))
        .unwrap_or(false);
    let ignorecase = config
        .as_ref()
        .and_then(|c| c.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);
    let needs_racy_content = paths.iter().any(|path| {
        index.get(path.as_bytes(), 0).is_some_and(|ie| {
            indexed_path_needs_worktree_stat_check(ie)
                && entry_is_racy_with_policy(ie, index_mtime, stat_policy)
        })
    });
    let mut stat_clean = true;
    if paths.len() < SWITCH_TREE_DIFF_STAT_SKIP_MIN {
        for path in paths {
            let Some(ie) = index.get(path.as_bytes(), 0) else {
                continue;
            };
            if !indexed_path_needs_worktree_stat_check(ie) || ie.mode != MODE_GITLINK {
                continue;
            }
            let abs = index_entry_worktree_abs(work_tree, path, precompose_unicode, ignorecase);
            let meta = match fs::symlink_metadata(&abs) {
                Ok(m) => m,
                Err(_) => {
                    stat_clean = false;
                    break;
                }
            };
            if !meta.is_dir() {
                stat_clean = false;
                break;
            }
        }
        if stat_clean {
            let blob_dirs = group_blob_entries_by_dir_for_paths(index, paths);
            let work_tree_owned = work_tree.to_path_buf();
            let dir_abs = |dir: &[u8]| {
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
            let file_abs = move |rel: &[u8]| {
                crate::diff::index_entry_worktree_abs_bytes(
                    &work_tree_for_files,
                    rel,
                    precompose_unicode,
                    ignorecase,
                )
            };
            for_each_blob_by_directory(
                index,
                &blob_dirs,
                dir_abs,
                file_abs,
                |_| false,
                |entry_index, lookup| {
                    if !stat_clean {
                        return Ok(());
                    }
                    let ie = &index.entries[entry_index];
                    match lookup {
                        BlobDiskLookup::Missing => stat_clean = false,
                        BlobDiskLookup::Io(e) => return Err(Error::Io(e)),
                        BlobDiskLookup::Present(stat) => {
                            if !crate::diff::stat_matches_blob_stat(ie, &stat, stat_policy) {
                                stat_clean = false;
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
    } else {
        let blob_dirs = group_blob_entries_by_dir_for_paths(index, paths);
        if blob_dirs.is_empty() {
            return Ok(true);
        }
        let preload_index = config
            .as_ref()
            .and_then(|c| c.get_bool("core.preloadIndex").and_then(|r| r.ok()))
            .unwrap_or(true);
        let stat_parallel_threads = Some(
            repo.thread_policy()
                .resolve(
                    ParallelOp::IndexRefresh,
                    config.as_deref().unwrap_or(&ConfigSet::new()),
                )
                .threads
                .get(),
        );
        let blob_scan = WorktreeBlobScanOptions {
            preload_index,
            stat_parallel_threads,
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
        let file_abs = move |rel: &[u8]| {
            crate::diff::index_entry_worktree_abs_bytes(
                &work_tree_for_files,
                rel,
                precompose_unicode,
                ignorecase,
            )
        };
        let matches = AtomicBool::new(true);
        let guard_batches = collect_blob_lookup_batches(
            index,
            &blob_dirs,
            blob_scan,
            &Default::default(),
            config.as_deref().unwrap_or(&ConfigSet::new()),
            false,
            None,
            dir_abs,
            file_abs,
            |_| false,
            BlobScanSparseMode::WorktreeGuard { stat_policy },
        )?;
        visit_blob_lookup_batches(guard_batches, |entry_index, lookup| {
            if !matches.load(Ordering::Relaxed) {
                return Ok(());
            }
            let ie = &index.entries[entry_index];
            if !indexed_path_needs_worktree_stat_check(ie) {
                return Ok(());
            }
            match lookup {
                BlobDiskLookup::Missing => {
                    matches.store(false, Ordering::Relaxed);
                }
                BlobDiskLookup::Io(e) => return Err(Error::Io(e)),
                BlobDiskLookup::Present(stat) => {
                    let mismatch = if ie.mode == MODE_GITLINK {
                        !stat.is_dir
                    } else if stat.git_mode != crate::index::MODE_SYMLINK
                        && stat.git_mode != crate::index::MODE_REGULAR
                        && stat.git_mode != crate::index::MODE_EXECUTABLE
                    {
                        true
                    } else {
                        !crate::diff::stat_matches_blob_stat(ie, &stat, stat_policy)
                    };
                    if mismatch {
                        matches.store(false, Ordering::Relaxed);
                    }
                }
            }
            Ok(())
        })?;
        stat_clean = matches.load(Ordering::Relaxed);
    }
    if !stat_clean {
        return Ok(false);
    }
    if !needs_racy_content {
        return Ok(true);
    }
    let cfg = config.unwrap_or_else(|| Arc::new(ConfigSet::new()));
    let conv = crlf::ConversionConfig::from_config(cfg.as_ref());
    let rules = WorktreeRules::from_parts_index_attributes_only(repo, index, cfg)?;
    let filters = repo.caches().filters();
    let odb = repo.odb();
    for path in paths {
        let Some(ie) = index.get(path.as_bytes(), 0) else {
            continue;
        };
        if !indexed_path_needs_worktree_stat_check(ie)
            || !entry_is_racy_with_policy(ie, index_mtime, stat_policy)
        {
            continue;
        }
        let abs = index_entry_worktree_abs(work_tree, path, precompose_unicode, ignorecase);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        if !stat_matches_with_policy(ie, &meta, stat_policy) {
            return Ok(false);
        }
        let attrs = rules.file_attrs(path, false);
        let content_ok = hash_worktree_file(
            odb,
            &abs,
            &meta,
            &conv,
            &attrs,
            path,
            Some(ie),
            Some(filters),
        )
        .is_ok_and(|oid| oid == ie.oid);
        if !content_ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn indexed_path_needs_worktree_stat_check(ie: &IndexEntry) -> bool {
    ie.stage() == 0 && !ie.skip_worktree() && !ie.assume_unchanged() && !ie.intent_to_add()
}

#[derive(Clone, Copy)]
enum HeadTreeSide {
    Present { oid: ObjectId, mode: u32 },
    Absent,
}

fn parse_diff_mode(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or(0)
}

fn head_tree_sides_from_changes(tree_changes: &[DiffEntry]) -> HashMap<String, HeadTreeSide> {
    let mut map = HashMap::with_capacity(tree_changes.len());
    for change in tree_changes {
        let Some(path) = &change.old_path else {
            continue;
        };
        map.insert(
            path.clone(),
            HeadTreeSide::Present {
                oid: change.old_oid,
                mode: parse_diff_mode(&change.old_mode),
            },
        );
    }
    map
}

pub(crate) fn paths_touched_by_tree_changes(tree_changes: &[DiffEntry]) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for change in tree_changes {
        if let Some(path) = &change.old_path {
            paths.insert(path.clone());
        }
        if let Some(path) = &change.new_path {
            paths.insert(path.clone());
        }
    }
    paths
}

fn path_is_tree_scheduled_delete(scheduled_delete_paths: &HashSet<String>, path: &str) -> bool {
    scheduled_delete_paths.contains(path)
}

/// Delete-only tree-diff paths whose index already matches HEAD and are not racy need no stat
/// before switch: a local edit would change stat data or show up in the staged pass.
/// Added tree-diff paths with no worktree file yet cannot have local edits to preserve.
fn added_tree_path_missing_on_disk(
    work_tree: &Path,
    added_tree_paths: &HashSet<String>,
    path: &str,
) -> bool {
    if !added_tree_paths.contains(path) {
        return false;
    }
    !work_tree.join(path).exists()
}

/// True when the path has no directory entry in the work tree (symlinks are not followed).
pub(crate) fn worktree_path_absent_for_trusted_delete(work_tree: &Path, path: &str) -> bool {
    match fs::symlink_metadata(work_tree.join(path)) {
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
        Ok(_) => false,
    }
}

fn index_matches_head_without_worktree_stat(
    index: &Index,
    head_by_path: &HashMap<String, HeadTreeSide>,
    path: &str,
    index_mtime: Option<(u32, u32)>,
    stat_policy: StatMatchPolicy,
) -> bool {
    let HeadTreeSide::Present { oid, mode } = head_by_path
        .get(path)
        .copied()
        .unwrap_or(HeadTreeSide::Absent)
    else {
        return false;
    };
    let Some(ie) = index.get(path.as_bytes(), 0) else {
        return false;
    };
    if ie.oid != oid || ie.mode != mode {
        return false;
    }
    !entry_is_racy_with_policy(ie, index_mtime, stat_policy)
}

fn delete_path_trusted_without_worktree_stat(
    work_tree: &Path,
    index: &Index,
    head_by_path: &HashMap<String, HeadTreeSide>,
    scheduled_delete_paths: &HashSet<String>,
    path: &str,
    index_mtime: Option<(u32, u32)>,
    stat_policy: StatMatchPolicy,
) -> bool {
    if !path_is_tree_scheduled_delete(scheduled_delete_paths, path) {
        return false;
    }
    if !index_matches_head_without_worktree_stat(
        index,
        head_by_path,
        path,
        index_mtime,
        stat_policy,
    ) {
        return false;
    }
    worktree_path_absent_for_trusted_delete(work_tree, path)
}

fn staged_diff_for_tree_switch_paths(
    index: &Index,
    head_by_path: &HashMap<String, HeadTreeSide>,
    paths: &BTreeSet<String>,
) -> Result<Vec<DiffEntry>> {
    let mut staged = Vec::new();
    for path in paths {
        let path_bytes = path.as_bytes();
        let index_entry = index.get(path_bytes, 0);
        if index_entry.is_some_and(|e| e.intent_to_add()) {
            continue;
        }
        let head = head_by_path
            .get(path)
            .copied()
            .unwrap_or(HeadTreeSide::Absent);
        match (head, index_entry) {
            (HeadTreeSide::Absent, None) => {}
            (HeadTreeSide::Absent, Some(ie)) => {
                staged.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path.clone()),
                    old_mode: String::new(),
                    new_mode: format_mode(ie.mode),
                    old_oid: ObjectId::zero(),
                    new_oid: ie.oid,
                    score: None,
                });
            }
            (HeadTreeSide::Present { oid, mode }, None) => {
                staged.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path.clone()),
                    new_path: None,
                    old_mode: format_mode(mode),
                    new_mode: String::new(),
                    old_oid: oid,
                    new_oid: ObjectId::zero(),
                    score: None,
                });
            }
            (HeadTreeSide::Present { oid, mode }, Some(ie)) if ie.oid != oid || ie.mode != mode => {
                staged.push(DiffEntry {
                    status: if ie.mode != mode {
                        DiffStatus::TypeChanged
                    } else {
                        DiffStatus::Modified
                    },
                    old_path: Some(path.clone()),
                    new_path: Some(path.clone()),
                    old_mode: format_mode(mode),
                    new_mode: format_mode(ie.mode),
                    old_oid: oid,
                    new_oid: ie.oid,
                    score: None,
                });
            }
            (HeadTreeSide::Present { .. }, Some(_)) => {}
        }
    }
    Ok(staged)
}

/// Pathspec roots covering every [`DiffStatus::Added`] path and its parent directories.
///
/// Used before tree checkout to find untracked paths that would be overwritten without scanning
/// the whole work tree.
#[must_use]
pub(crate) fn untracked_scan_pathspecs_for_tree_changes(tree_changes: &[DiffEntry]) -> Vec<String> {
    let mut specs = BTreeSet::new();
    for change in tree_changes {
        if change.status != DiffStatus::Added {
            continue;
        }
        let Some(added) = &change.new_path else {
            continue;
        };
        let trimmed = added.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        if let Some((top, _rest)) = trimmed.split_once('/') {
            specs.insert(top.to_owned());
        } else {
            specs.insert(trimmed.to_owned());
        }
    }
    specs.into_iter().collect()
}

/// Pathspec roots for deleted paths (parent directories), used before wholesale `remove_dir_all`.
#[must_use]
pub(crate) fn untracked_scan_pathspecs_for_deletes(tree_changes: &[DiffEntry]) -> Vec<String> {
    let mut specs = BTreeSet::new();
    for change in tree_changes {
        if change.status != DiffStatus::Deleted {
            continue;
        }
        let Some(deleted) = &change.old_path else {
            continue;
        };
        let trimmed = deleted.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        if let Some((top, _rest)) = trimmed.split_once('/') {
            specs.insert(top.to_owned());
        } else {
            specs.insert(trimmed.to_owned());
        }
    }
    specs.into_iter().collect()
}

/// Exact added paths (and ancestor directories) for untracked overwrite checks during fast wholesale scan.
///
/// Top-level pathspec roots would walk entire trees under each root; this list stays proportional to
/// the number of added paths in the tree diff.
#[must_use]
pub(crate) fn untracked_scan_paths_for_added_tree_changes(
    tree_changes: &[DiffEntry],
) -> Vec<String> {
    let mut specs = BTreeSet::new();
    for change in tree_changes {
        if change.status != DiffStatus::Added {
            continue;
        }
        let Some(added) = &change.new_path else {
            continue;
        };
        let trimmed = added.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        specs.insert(trimmed.to_owned());
        let mut rest = trimmed;
        while let Some((prefix, _)) = rest.rsplit_once('/') {
            specs.insert(prefix.to_owned());
            rest = prefix;
        }
    }
    specs.into_iter().collect()
}

const WHOLESALE_FAST_UNTRACKED_SCAN_MIN: usize = 32;
const SWITCH_TREE_DIFF_STAT_SKIP_MIN: usize = 128;

fn delete_paths_from_tree_changes(tree_changes: &[DiffEntry]) -> HashSet<String> {
    tree_changes
        .iter()
        .filter(|c| c.status == DiffStatus::Deleted)
        .filter_map(|c| c.old_path.clone())
        .collect()
}

fn delete_path_scheduled_for_removal(
    rel: &str,
    delete_paths: &HashSet<String>,
    ignorecase: bool,
) -> bool {
    if ignorecase {
        delete_paths
            .iter()
            .any(|del| crate::path_icase::paths_equal(rel.as_bytes(), del.as_bytes(), true))
    } else {
        delete_paths.contains(rel)
    }
}

/// Scan wholesale-delete directory trees for extra untracked/ignored files without a full status walk.
///
/// Every on-disk file under `delete_roots` must appear in `delete_paths` (scheduled removal). Any
/// other file is classified with the same ignore rules as [`collect_untracked_and_ignored`].
struct WholesaleDeleteRootScan {
    untracked: Vec<String>,
    needs_ignore_rules: Vec<String>,
}

fn index_entry_under_root(path: &[u8], root: &[u8]) -> bool {
    path == root || path.get(root.len()).is_some_and(|b| *b == b'/') && path.starts_with(root)
}

fn count_indexed_paths_under_root(index: &Index, root: &str) -> usize {
    let root_bytes = root.as_bytes();
    let prefix = format!("{root}/");
    let prefix_bytes = prefix.as_bytes();
    if index.entries_sorted {
        let mut count = 0usize;
        let start = index
            .entries
            .partition_point(|e| e.path.as_slice() < prefix_bytes);
        for entry in &index.entries[start..] {
            if !entry.path.starts_with(prefix_bytes) {
                break;
            }
            if entry.stage() == 0 {
                count += 1;
            }
        }
        if let Ok(pos) = index
            .entries
            .binary_search_by(|e| e.path.as_slice().cmp(root_bytes))
        {
            if index.entries[pos].stage() == 0 {
                count += 1;
            }
        }
        return count;
    }
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .filter(|e| index_entry_under_root(&e.path, root_bytes))
        .count()
}

fn delete_path_under_root(path: &str, root: &str, ignorecase: bool) -> bool {
    if ignorecase {
        path.eq_ignore_ascii_case(root)
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    } else {
        path == root
            || (path.len() > root.len()
                && path.as_bytes().get(root.len()) == Some(&b'/')
                && path.starts_with(root))
    }
}

fn rel_path_under_worktree(work_tree: &Path, abs: &Path, rel_buf: &mut String) {
    rel_buf.clear();
    let Ok(rest) = abs.strip_prefix(work_tree) else {
        return;
    };
    let mut first = true;
    for component in rest.components() {
        if let std::path::Component::Normal(part) = component {
            if !first {
                rel_buf.push('/');
            }
            rel_buf.push_str(&part.to_string_lossy());
            first = false;
        }
    }
}

fn precompute_wholesale_root_index_delete_counts(
    index: &Index,
    delete_paths: &HashSet<String>,
    delete_roots: &BTreeSet<String>,
    ignorecase: bool,
) -> HashMap<String, (usize, usize)> {
    let mut counts: HashMap<String, (usize, usize)> = delete_roots
        .iter()
        .map(|root| (root.clone(), (0usize, 0usize)))
        .collect();
    if !ignorecase && index.entries_sorted {
        for root in delete_roots {
            if let Some(slot) = counts.get_mut(root) {
                slot.0 = count_indexed_paths_under_root(index, root);
            }
        }
    } else {
        for entry in index.entries.iter().filter(|e| e.stage() == 0) {
            let Ok(path) = std::str::from_utf8(&entry.path) else {
                continue;
            };
            let root = path.split('/').next().unwrap_or(path);
            if let Some(slot) = counts.get_mut(root) {
                if index_entry_under_root(path.as_bytes(), root.as_bytes()) {
                    slot.0 += 1;
                }
            } else if ignorecase {
                if let Some(key) = counts
                    .keys()
                    .find(|candidate| candidate.eq_ignore_ascii_case(root))
                    .cloned()
                {
                    if path.eq_ignore_ascii_case(&key)
                        || path
                            .strip_prefix(key.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                    {
                        if let Some(slot) = counts.get_mut(&key) {
                            slot.0 += 1;
                        }
                    }
                }
            }
        }
    }
    for del in delete_paths {
        let root = del.split('/').next().unwrap_or(del.as_str());
        if let Some(slot) = counts.get_mut(root) {
            slot.1 += 1;
        } else if ignorecase {
            if let Some(key) = counts
                .keys()
                .find(|candidate| candidate.eq_ignore_ascii_case(root))
                .cloned()
            {
                if let Some(slot) = counts.get_mut(&key) {
                    slot.1 += 1;
                }
            }
        }
    }
    counts
}

fn count_delete_paths_under_root(
    delete_paths: &HashSet<String>,
    root: &str,
    ignorecase: bool,
) -> usize {
    delete_paths
        .iter()
        .filter(|p| delete_path_under_root(p, root, ignorecase))
        .count()
}

fn scan_wholesale_delete_root(
    index: &Index,
    work_tree: &Path,
    root: &str,
    delete_paths: &HashSet<String>,
    ignorecase: bool,
    precomputed_counts: Option<(usize, usize)>,
) -> WholesaleDeleteRootScan {
    let (indexed, deletes) = precomputed_counts.unwrap_or_else(|| {
        (
            count_indexed_paths_under_root(index, root),
            count_delete_paths_under_root(delete_paths, root, ignorecase),
        )
    });
    let index_aligned = indexed > 0 && indexed == deletes;
    let mut untracked = Vec::new();
    let mut needs_ignore_rules = Vec::new();
    let abs_root = work_tree.join(root);
    if !abs_root.is_dir() {
        return WholesaleDeleteRootScan {
            untracked,
            needs_ignore_rules,
        };
    }
    let mut rel_buf = String::with_capacity(root.len() + 64);
    rel_buf.push_str(root);
    let mut stack: Vec<PathBuf> = vec![abs_root];
    while let Some(abs) = stack.pop() {
        rel_path_under_worktree(work_tree, &abs, &mut rel_buf);
        let read_dir = match fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel_start = rel_buf.len();
            if rel_start > 0 {
                rel_buf.push('/');
            }
            rel_buf.push_str(&name);
            let rel = rel_buf.as_str();
            if index_aligned && delete_path_scheduled_for_removal(rel, delete_paths, ignorecase) {
                // Scheduled file deletes need no scan work. Only directory paths in the delete
                // set require descending to discover blocking special files.
                let scheduled_dir = delete_paths.contains(rel)
                    || (ignorecase && delete_paths.iter().any(|del| del.eq_ignore_ascii_case(rel)));
                if scheduled_dir {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if is_dir {
                        stack.push(entry.path());
                    }
                }
                rel_buf.truncate(rel_start);
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => {
                    rel_buf.truncate(rel_start);
                    continue;
                }
            };
            if file_type.is_dir() {
                stack.push(entry.path());
                rel_buf.truncate(rel_start);
                continue;
            }
            if !file_type.is_file() && !file_type.is_symlink() {
                if delete_path_scheduled_for_removal(rel, delete_paths, ignorecase) {
                    rel_buf.truncate(rel_start);
                    continue;
                }
                if !index_aligned && index.get(rel.as_bytes(), 0).is_some() {
                    rel_buf.truncate(rel_start);
                    continue;
                }
                // FIFOs, sockets, and devices are not deleted wholesale; block trusted removal.
                untracked.push(rel.to_owned());
                rel_buf.truncate(rel_start);
                continue;
            }
            if delete_path_scheduled_for_removal(rel, delete_paths, ignorecase) {
                rel_buf.truncate(rel_start);
                continue;
            }
            if index_aligned {
                needs_ignore_rules.push(rel.to_owned());
                rel_buf.truncate(rel_start);
                continue;
            }
            if index.get(rel.as_bytes(), 0).is_some() {
                rel_buf.truncate(rel_start);
                continue;
            }
            needs_ignore_rules.push(rel.to_owned());
            rel_buf.truncate(rel_start);
        }
    }
    WholesaleDeleteRootScan {
        untracked,
        needs_ignore_rules,
    }
}

fn collect_untracked_for_wholesale_delete_roots(
    repo: &Repository,
    index: &Index,
    work_tree: &Path,
    delete_paths: &HashSet<String>,
    delete_roots: &BTreeSet<String>,
    ignorecase: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let root_counts = precompute_wholesale_root_index_delete_counts(
        index,
        delete_paths,
        delete_roots,
        ignorecase,
    );
    let roots: Vec<String> = delete_roots.iter().cloned().collect();
    let work_tree_owned = work_tree.to_path_buf();
    let delete_paths_shared = Arc::new(delete_paths.clone());
    const PARALLEL_WHOLESALE_ROOT_SCAN_MIN: usize = 16;
    let scans: Vec<WholesaleDeleteRootScan> = if roots.len() >= PARALLEL_WHOLESALE_ROOT_SCAN_MIN {
        let config = repo.config().ok();
        let default_cfg = ConfigSet::new();
        let cfg = config.as_deref().unwrap_or(&default_cfg);
        let options = repo.thread_policy().resolve(ParallelOp::Checkout, cfg);
        let scan_outcomes = try_par_map(&roots, options, 0, |root| {
            let counts = root_counts.get(root).copied();
            Ok(scan_wholesale_delete_root(
                index,
                &work_tree_owned,
                root,
                delete_paths_shared.as_ref(),
                ignorecase,
                counts,
            ))
        })
        .unwrap_or_else(|_: ParallelHashError<()>| {
            roots
                .iter()
                .map(|root| {
                    scan_wholesale_delete_root(
                        index,
                        work_tree,
                        root,
                        delete_paths,
                        ignorecase,
                        root_counts.get(root).copied(),
                    )
                })
                .collect()
        });
        scan_outcomes
    } else {
        roots
            .iter()
            .map(|root| {
                scan_wholesale_delete_root(
                    index,
                    work_tree,
                    root,
                    delete_paths,
                    ignorecase,
                    root_counts.get(root).copied(),
                )
            })
            .collect()
    };

    let mut untracked = Vec::new();
    let mut needs_ignore_rules = Vec::new();
    for scan in scans {
        untracked.extend(scan.untracked);
        needs_ignore_rules.extend(scan.needs_ignore_rules);
    }

    if needs_ignore_rules.is_empty() {
        return Ok((untracked, Vec::new()));
    }

    let config = ConfigSet::load(repo.environment(), Some(repo.git_dir()), true)
        .ok()
        .map(Arc::new)
        .unwrap_or_else(|| Arc::new(ConfigSet::new()));
    let rules = WorktreeRules::from_parts_index_attributes_only(repo, index, config)?;
    let mut matcher = rules.ignore_mut();
    let mut ignored = Vec::new();
    for rel in needs_ignore_rules {
        if matcher.is_path_ignored(repo, Some(index), &rel, false)? {
            ignored.push(rel);
        } else {
            untracked.push(rel);
        }
    }
    Ok((untracked, ignored))
}

pub(crate) fn collect_untracked_for_tree_changes(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    tree_changes: &[DiffEntry],
) -> Result<Vec<String>> {
    let (untracked, _, _, _) = collect_untracked_and_wholesale_trust_for_tree_changes(
        repo,
        index,
        work_tree,
        tree_changes,
        None,
    )?;
    Ok(untracked)
}

/// Untracked paths for added/deleted tree diff paths, ignored paths when needed, and whether
/// switch checkout may skip wholesale on-disk safety walks.
pub(crate) fn collect_untracked_and_wholesale_trust_for_tree_changes(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    tree_changes: &[DiffEntry],
    precomputed_delete_paths: Option<&HashSet<String>>,
) -> Result<(Vec<String>, Vec<String>, bool, bool)> {
    let added_specs = untracked_scan_pathspecs_for_tree_changes(tree_changes);
    let delete_specs = untracked_scan_pathspecs_for_deletes(tree_changes);
    let delete_roots: BTreeSet<String> = delete_specs.iter().cloned().collect();
    let mut pathspecs: BTreeSet<String> = added_specs.iter().cloned().collect();
    pathspecs.extend(delete_specs.iter().cloned());
    if pathspecs.is_empty() {
        return Ok((Vec::new(), Vec::new(), true, false));
    }
    let all_specs: Vec<String> = pathspecs.into_iter().collect();
    let ignorecase = repo
        .config()
        .ok()
        .and_then(|c| c.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);
    let under_delete_root = |path: &str| {
        let root = path.split('/').next().unwrap_or(path);
        if ignorecase {
            delete_roots
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(root))
        } else {
            delete_roots.contains(root)
        }
    };
    let delete_paths_owned;
    let delete_paths = match precomputed_delete_paths {
        Some(set) => set,
        None => {
            delete_paths_owned = delete_paths_from_tree_changes(tree_changes);
            &delete_paths_owned
        }
    };
    let use_fast_wholesale_scan = !delete_roots.is_empty()
        && delete_paths.len() >= WHOLESALE_FAST_UNTRACKED_SCAN_MIN
        && delete_roots
            .iter()
            .all(|root| !root.is_empty() && !root.contains('/'));
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    if use_fast_wholesale_scan {
        let (u, i) = collect_untracked_for_wholesale_delete_roots(
            repo,
            index,
            work_tree,
            delete_paths,
            &delete_roots,
            ignorecase,
        )?;
        untracked.extend(u);
        ignored.extend(i);
    }
    let scan_specs: Vec<String> = if use_fast_wholesale_scan {
        untracked_scan_paths_for_added_tree_changes(tree_changes)
            .into_iter()
            .filter(|spec| work_tree.join(spec).exists())
            .collect()
    } else {
        all_specs.clone()
    };
    if !scan_specs.is_empty() {
        let (u, mut ign) = collect_untracked_and_ignored_with_cache(
            repo,
            index,
            work_tree,
            IgnoredMode::No,
            true,
            &scan_specs,
            true,
        )?;
        untracked.extend(u);
        ignored.append(&mut ign);
    }
    let untracked_blocks_wholesale =
        !delete_roots.is_empty() && untracked.iter().any(|p| under_delete_root(p));
    let need_traditional_ignored =
        !delete_roots.is_empty() && !untracked_blocks_wholesale && !use_fast_wholesale_scan;
    if need_traditional_ignored {
        let (_, ignored_under_deletes) = collect_untracked_and_ignored_with_cache(
            repo,
            index,
            work_tree,
            IgnoredMode::Traditional,
            true,
            &all_specs,
            true,
        )?;
        ignored = ignored_under_deletes;
    }
    let trust_wholesale = delete_roots.is_empty()
        || (!untracked_blocks_wholesale && !ignored.iter().any(|p| under_delete_root(p)));
    Ok((
        untracked,
        ignored,
        trust_wholesale,
        need_traditional_ignored,
    ))
}

fn ensure_worktree_clean_with_message(
    repo: &Repository,
    message: &str,
    persist_index_stat_refresh: bool,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    ensure_worktree_clean_from_snapshot(repo, &mut snapshot, message, persist_index_stat_refresh)?;
    Ok(())
}

/// Refuse when staged or unstaged changes exist, reusing an already-loaded snapshot.
pub(crate) fn ensure_worktree_clean_from_snapshot(
    repo: &Repository,
    snapshot: &mut WorktreeSnapshot,
    message: &str,
    persist_index_stat_refresh: bool,
) -> Result<()> {
    let index_path = repo.index_path_for_env()?;
    let (staged, unstaged, index_changed) =
        local_change_diffs(repo, &mut snapshot.index, snapshot.head_tree)?;
    if persist_index_stat_refresh
        && index_changed
        && repo.try_write_index_at(&index_path, &mut snapshot.index)?
    {
        snapshot.index.source_mtime = index_file_mtime(&index_path);
    }
    if !staged.is_empty() || !unstaged.is_empty() {
        return Err(Error::Message(message.into()));
    }
    Ok(())
}

/// Cleanliness check for pick/revert that hands back the loaded snapshot for the replay steps.
///
/// Returns `None` for bare repositories.
pub(crate) fn prepare_worktree_for_pick(repo: &Repository) -> Result<Option<WorktreeSnapshot>> {
    if repo.work_tree().is_none() {
        return Ok(None);
    }
    let mut snapshot = load_worktree_snapshot(repo)?;
    ensure_worktree_clean_from_snapshot(
        repo,
        &mut snapshot,
        "you have uncommitted changes — commit them before picking",
        false,
    )?;
    Ok(Some(snapshot))
}

/// Refuse when the index or worktree differs from HEAD (staged or unstaged changes).
///
/// # Errors
///
/// Returns [`Error::Message`] when changes are present, matching `grit pick` wording.
pub fn ensure_worktree_clean_for_pick(repo: &Repository) -> Result<()> {
    ensure_worktree_clean_with_message(
        repo,
        "you have uncommitted changes — commit them before picking",
        false,
    )
}

/// Refuse when the index or worktree differs from HEAD (staged or unstaged changes).
///
/// # Errors
///
/// Returns [`Error::Message`] when changes are present, matching `grit merge` wording.
pub fn ensure_worktree_clean_for_merge(repo: &Repository) -> Result<()> {
    ensure_worktree_clean_with_message(
        repo,
        "you have uncommitted changes — commit them before merging",
        true,
    )
}

/// Refuse when the index or worktree differs from `HEAD` before `rebase --exec`.
///
/// # Errors
///
/// Returns [`Error::Message`] when staged or unstaged changes are present.
pub fn ensure_worktree_clean_for_rebase_exec(repo: &Repository) -> Result<()> {
    ensure_worktree_clean_with_message(
        repo,
        "you have uncommitted changes — commit them before rebasing with --exec",
        false,
    )
}

/// Inputs for a branch switch or fast-forward checkout after cleanliness checks.
pub struct TreeSwitchPlan {
    /// Untracked paths under the work tree (normal untracked mode).
    pub untracked: Vec<String>,
    /// Tree diff from `from_tree` to `to_tree`.
    pub tree_changes: Vec<DiffEntry>,
}

/// Verify the worktree is clean, collect untracked paths, and diff two trees in one index load.
///
/// # Errors
///
/// Returns [`Error::Message`] when staged or unstaged changes exist, or on I/O/ODB failures.
pub fn prepare_tree_switch(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<TreeSwitchPlan> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let index_path = repo.index_path_for_env()?;
    let (staged, unstaged, index_changed) =
        local_change_diffs(repo, &mut snapshot.index, snapshot.head_tree)?;
    if index_changed && repo.try_write_index_at(&index_path, &mut snapshot.index)? {
        snapshot.index.source_mtime = index_file_mtime(&index_path);
    }
    if !staged.is_empty() || !unstaged.is_empty() {
        return Err(Error::Message(
            "you have uncommitted changes — commit them before switching".into(),
        ));
    }

    let tree_changes = if from_tree == Some(to_tree) {
        Vec::new()
    } else {
        diff_trees(repo.odb(), from_tree, Some(to_tree), "")?
    };
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, &tree_changes)?;
    Ok(TreeSwitchPlan {
        untracked,
        tree_changes,
    })
}

/// `true` when `prefix` is a proper path prefix of `path` at a `/` component boundary.
fn is_component_prefix(prefix: &str, path: &str) -> bool {
    if path.len() <= prefix.len() {
        return false;
    }
    path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// Whether an added tree path would replace untracked content at `untracked` (exact or file/dir clash).
fn added_path_collides_with_untracked(added: &str, untracked: &str) -> bool {
    added == untracked
        || is_component_prefix(added, untracked)
        || is_component_prefix(untracked, added)
}

/// Paths of untracked worktree files that a tree checkout would replace.
///
/// Only tree diff entries with [`DiffStatus::Added`] are considered: the destination
/// tree introduces a new path that is not present in the source tree. Collisions include
/// exact matches and file/directory clashes at component boundaries (e.g. added `slot` vs
/// untracked `slot/precious.txt`, or the reverse).
#[must_use]
pub fn untracked_would_be_overwritten_by_tree_changes(
    untracked_paths: &[String],
    tree_changes: &[DiffEntry],
) -> Vec<String> {
    if untracked_paths.is_empty() {
        return Vec::new();
    }
    let mut collisions = Vec::new();
    for change in tree_changes {
        if change.status != DiffStatus::Added {
            continue;
        }
        let Some(added) = &change.new_path else {
            continue;
        };
        for untracked in untracked_paths {
            let u = untracked.trim_end_matches('/');
            if u.is_empty() {
                continue;
            }
            if added_path_collides_with_untracked(added, u) {
                collisions.push(u.to_owned());
            }
        }
    }
    collisions.sort();
    collisions.dedup();
    collisions
}

/// Refuse when [`untracked_would_be_overwritten_by_tree_changes`] would be non-empty.
///
/// # Errors
///
/// Returns [`Error::Message`] naming the first colliding path (same wording as `grit switch`).
pub fn ensure_no_untracked_overwrite(
    untracked_paths: &[String],
    tree_changes: &[DiffEntry],
) -> Result<()> {
    let collisions = untracked_would_be_overwritten_by_tree_changes(untracked_paths, tree_changes);
    if let Some(path) = collisions.first() {
        return Err(Error::Message(format!(
            "untracked file '{path}' would be overwritten — move or remove it first"
        )));
    }
    Ok(())
}

/// Collect untracked paths and refuse when updating the worktree from `from_tree` to `to_tree`
/// would clobber an untracked file.
///
/// Does not check for staged or unstaged changes; callers such as `grit merge` enforce scoped
/// local-change compatibility separately.
///
/// # Errors
///
/// Returns [`Error::Message`] when an untracked path would be overwritten, or on I/O/ODB failures.
pub fn prepare_tree_checkout(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<()> {
    let tree_changes = diff_trees(repo.odb(), from_tree, Some(to_tree), "")?;
    prepare_tree_checkout_with_changes(repo, &tree_changes)
}

/// Untracked guard for a precomputed tree-to-tree diff (avoids a second full-tree diff).
///
/// # Errors
///
/// Returns [`Error::Message`] when an untracked path would be overwritten.
pub(crate) fn prepare_tree_checkout_with_changes(
    repo: &Repository,
    tree_changes: &[DiffEntry],
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, tree_changes)?;
    ensure_no_untracked_overwrite(&untracked, tree_changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::diff_trees;
    use crate::repo::{init_repository, Repository};
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        std::fs::write(dir.path().join("tracked.txt"), "v1\n").expect("write");
        (dir, repo)
    }

    fn added_entry(path: &str) -> DiffEntry {
        DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(path.to_owned()),
            old_mode: String::new(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score: None,
        }
    }

    fn modified_entry(path: &str) -> DiffEntry {
        DiffEntry {
            status: DiffStatus::Modified,
            old_path: Some(path.to_owned()),
            new_path: Some(path.to_owned()),
            old_mode: "100644".into(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score: None,
        }
    }

    #[test]
    fn untracked_collision_only_on_added_paths() {
        let untracked = vec!["new.txt".into(), "other.txt".into()];
        let changes = vec![
            added_entry("new.txt"),
            modified_entry("tracked.txt"),
            added_entry("missing.txt"),
        ];
        let hits = untracked_would_be_overwritten_by_tree_changes(&untracked, &changes);
        assert_eq!(hits, vec!["new.txt".to_owned()]);
    }

    #[test]
    fn untracked_collision_sorted_and_multiple() {
        let untracked = vec!["b.txt".into(), "a.txt".into()];
        let changes = vec![added_entry("b.txt"), added_entry("a.txt")];
        let hits = untracked_would_be_overwritten_by_tree_changes(&untracked, &changes);
        assert_eq!(hits, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
    }

    #[test]
    fn untracked_collision_file_over_untracked_directory() {
        let untracked = vec!["slot/precious.txt".into()];
        let hits =
            untracked_would_be_overwritten_by_tree_changes(&untracked, &[added_entry("slot")]);
        assert_eq!(hits, vec!["slot/precious.txt".to_owned()]);
    }

    #[test]
    fn untracked_collision_directory_over_untracked_file() {
        let untracked = vec!["slot".into()];
        let hits = untracked_would_be_overwritten_by_tree_changes(
            &untracked,
            &[added_entry("slot/incoming.txt")],
        );
        assert_eq!(hits, vec!["slot".to_owned()]);
    }

    #[test]
    fn untracked_collision_allows_sibling_paths() {
        let untracked = vec!["slot/precious.txt".into()];
        let hits = untracked_would_be_overwritten_by_tree_changes(
            &untracked,
            &[added_entry("slot/incoming.txt")],
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn untracked_scan_pathspecs_include_added_and_parents() {
        let changes = vec![
            added_entry("dir/sub/new.txt"),
            modified_entry("tracked.txt"),
            added_entry("top"),
        ];
        let specs = super::untracked_scan_pathspecs_for_tree_changes(&changes);
        assert!(specs.contains(&"dir".to_owned()));
        assert!(!specs.contains(&"dir/sub/new.txt".to_owned()));
        assert!(specs.contains(&"top".to_owned()));
        assert!(!specs.contains(&"tracked.txt".to_owned()));
    }

    #[test]
    fn untracked_scan_paths_for_added_include_path_and_ancestors() {
        let changes = vec![
            added_entry("dir/sub/new.txt"),
            modified_entry("tracked.txt"),
            added_entry("top"),
        ];
        let specs = super::untracked_scan_paths_for_added_tree_changes(&changes);
        assert!(specs.contains(&"dir/sub/new.txt".to_owned()));
        assert!(specs.contains(&"dir/sub".to_owned()));
        assert!(specs.contains(&"dir".to_owned()));
        assert!(specs.contains(&"top".to_owned()));
        assert!(!specs.contains(&"tracked.txt".to_owned()));
    }

    #[test]
    fn ensure_no_untracked_overwrite_err_message() {
        let err = ensure_no_untracked_overwrite(&["x.txt".into()], &[added_entry("x.txt")])
            .expect_err("collision");
        match err {
            Error::Message(msg) => {
                assert!(msg.contains("untracked file 'x.txt' would be overwritten"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn fast_wholesale_delete_untracked_scan_ignores_scheduled_paths() {
        let (tmp, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        std::fs::create_dir_all(wt.join("bulk")).expect("mkdir");
        let mut delete_paths = HashSet::new();
        for i in 0..40 {
            let rel = format!("bulk/f{i:02}.txt");
            std::fs::write(wt.join(&rel), format!("body {i}\n")).expect("write");
            delete_paths.insert(rel);
        }
        let delete_roots: BTreeSet<String> = ["bulk".into()].into_iter().collect();
        let index = repo.load_index().expect("index");
        let (untracked, ignored) = collect_untracked_for_wholesale_delete_roots(
            &repo,
            &index,
            wt,
            &delete_paths,
            &delete_roots,
            false,
        )
        .expect("scan");
        assert!(untracked.is_empty(), "got {untracked:?}");
        assert!(ignored.is_empty(), "got {ignored:?}");
        std::fs::write(wt.join("bulk/extra.txt"), b"x\n").expect("extra");
        let (untracked, _) = collect_untracked_for_wholesale_delete_roots(
            &repo,
            &index,
            wt,
            &delete_paths,
            &delete_roots,
            false,
        )
        .expect("scan again");
        assert!(
            untracked.iter().any(|p| p == "bulk/extra.txt"),
            "expected extra untracked, got {untracked:?}"
        );
        let _ = tmp;
    }

    #[test]
    fn local_change_diffs_for_tree_changes_matches_full_on_modified_path() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        let mut index = repo.load_index().expect("index");
        let data = std::fs::read(wt.join("tracked.txt")).expect("read");
        let oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, &data)
            .expect("blob");
        let entry = crate::index::entry_from_stat(
            &wt.join("tracked.txt"),
            b"tracked.txt",
            oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        repo.write_index(&mut index).expect("write index");
        let head_tree = crate::write_tree::write_tree_update_index(
            &repo.odb(),
            &mut index,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .expect("tree");

        std::fs::write(wt.join("other.txt"), b"new\n").expect("write other");
        let other_data = b"new\n";
        let other_oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, other_data)
            .expect("blob");
        let mut index2 = repo.load_index().expect("index");
        let other_entry = crate::index::entry_from_stat(
            &wt.join("other.txt"),
            b"other.txt",
            other_oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index2.add_or_replace(other_entry);
        index2.sort();
        let target_tree = crate::write_tree::write_tree_update_index(
            &repo.odb(),
            &mut index2,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .expect("target tree");

        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");
        let mut index3 = repo.load_index().expect("index");
        let (full_staged, full_unstaged, _) =
            local_change_diffs(&repo, &mut index3, Some(head_tree)).expect("full");
        let mut index4 = repo.load_index().expect("index");
        let (scoped_staged, scoped_unstaged, _) = local_change_diffs_for_tree_changes(
            &repo,
            &mut index4,
            Some(head_tree),
            &tree_changes,
            None,
        )
        .expect("scoped");
        assert_eq!(full_staged, scoped_staged);
        assert_eq!(full_unstaged, scoped_unstaged);
    }

    fn gitlink_index_entry(path: &str, oid: ObjectId) -> crate::index::IndexEntry {
        use crate::index::{IndexEntry, MODE_GITLINK};
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_GITLINK,
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

    #[test]
    fn touched_gitlink_skips_stat_only_fast_path() {
        use crate::objects::{serialize_commit, CommitData, ObjectKind};
        use crate::refs;
        use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
        use std::process::Command;

        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        fn git_at(dir: &std::path::Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?} in {} failed: {}",
                dir.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let repo = init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        let sub = root.join("inner");
        std::fs::create_dir_all(&sub).expect("mkdir inner");
        git_at(&sub, &["init", "-q", "-b", "main"]);
        git_at(&sub, &["config", "user.email", "t@t"]);
        git_at(&sub, &["config", "user.name", "T"]);
        std::fs::write(sub.join("f"), b"v1\n").expect("write");
        git_at(&sub, &["add", "f"]);
        git_at(&sub, &["commit", "-qm", "v1"]);
        let sub1 = ObjectId::from_hex(git_at(&sub, &["rev-parse", "HEAD"]).trim()).expect("sub1");
        std::fs::write(sub.join("f"), b"v1\nv2\n").expect("v2");
        git_at(&sub, &["add", "f"]);
        git_at(&sub, &["commit", "-qm", "v2"]);
        let sub2 = ObjectId::from_hex(git_at(&sub, &["rev-parse", "HEAD"]).trim()).expect("sub2");
        git_at(&sub, &["checkout", "-q", &sub1.to_hex()]);
        let mut index = repo.load_index().expect("index");
        index.add_or_replace(gitlink_index_entry("inner", sub1));
        index.sort();
        let head_tree =
            write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                .expect("head tree");
        repo.write_index(&mut index).expect("write index");
        let commit = CommitData {
            tree: head_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "main\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "HEAD", &co).expect("head");

        git_at(&sub, &["checkout", "-q", &sub2.to_hex()]);
        let mut target_index = index.clone();
        target_index.add_or_replace(gitlink_index_entry("inner", sub2));
        target_index.sort();
        let target_tree =
            write_tree_update_index(&repo.odb(), &mut target_index, "", WriteTreeFlags::silent())
                .expect("target tree");
        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");
        let mut index_probe = repo.load_index().expect("index");
        let (_, unstaged, _) = local_change_diffs_for_tree_changes(
            &repo,
            &mut index_probe,
            Some(head_tree),
            &tree_changes,
            None,
        )
        .expect("scoped diffs");
        assert!(
            unstaged
                .iter()
                .any(|e| e.old_path.as_deref() == Some("inner")),
            "gitlink HEAD mismatch must not pass stat-only fast path, got {unstaged:?}"
        );
    }

    #[test]
    fn switch_dirty_racy_same_size_content_is_not_clean() {
        use crate::objects::{serialize_commit, CommitData, ObjectKind};
        use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
        use crate::refs;
        use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let repo = init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        std::fs::write(root.join("tracked.txt"), b"AAAA\n").expect("write a");
        let mut index = repo.load_index().expect("index");
        let abs = root.join("tracked.txt");
        let oid_a = repo.odb().write(ObjectKind::Blob, b"AAAA\n").expect("blob");
        let entry_a =
            crate::index::entry_from_stat(&abs, b"tracked.txt", oid_a, crate::index::MODE_REGULAR)
                .expect("entry");
        index.add_or_replace(entry_a.clone());
        index.sort();
        let tree_a = write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .expect("tree a");
        repo.write_index(&mut index).expect("write index");
        let commit_a = CommitData {
            tree: tree_a,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "a\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let tip_a = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit_a))
            .expect("commit a");
        refs::write_ref(&repo.git_dir(), "refs/heads/first", &tip_a).expect("first");
        refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/first").expect("head");

        let mut index_on_first = repo.load_index().expect("reload first");
        std::fs::write(&abs, b"AAAA\n").expect("restore a on disk");
        index_on_first.entries[0].oid = oid_a;
        index_on_first.sort();
        repo.write_index(&mut index_on_first)
            .expect("sync index to first");

        std::fs::write(&abs, b"BBBB\n").expect("write b on other branch");
        let oid_b = repo
            .odb()
            .write(ObjectKind::Blob, b"BBBB\n")
            .expect("blob b");
        let mut index_b = repo.load_index().expect("index for b");
        index_b.add_or_replace(
            crate::index::entry_from_stat(&abs, b"tracked.txt", oid_b, crate::index::MODE_REGULAR)
                .expect("entry b"),
        );
        index_b.sort();
        let tree_b =
            write_tree_update_index(&repo.odb(), &mut index_b, "", WriteTreeFlags::silent())
                .expect("tree b");
        assert_ne!(tree_a, tree_b, "branch trees must differ");
        let commit_b = CommitData {
            tree: tree_b,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "b\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let tip_b = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit_b))
            .expect("commit b");
        refs::write_ref(&repo.git_dir(), "refs/heads/other", &tip_b).expect("other");
        std::fs::write(&abs, b"AAAA\n").expect("back to first branch content");
        let mut index_back = repo.load_index().expect("index back on first");
        index_back.add_or_replace(
            crate::index::entry_from_stat(&abs, b"tracked.txt", oid_a, crate::index::MODE_REGULAR)
                .expect("entry a again"),
        );
        index_back.sort();
        repo.write_index(&mut index_back)
            .expect("index synced to first");

        let index_path = repo.index_path_for_env().expect("index path");
        let pinned = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&abs, pinned).expect("pin file");
        filetime::set_file_mtime(&index_path, pinned).expect("pin index");
        std::fs::write(&abs, b"DIRT\n").expect("same-size dirty");
        filetime::set_file_mtime(&abs, pinned).expect("forge file stat to match index");
        let mut index_dirty = repo.load_index().expect("reload");
        index_dirty.entries[0].oid = oid_a;
        index_dirty.entries[0].mtime_sec = 1_700_000_000;
        index_dirty.entries[0].mtime_nsec = 0;
        index_dirty.entries[0].size = 5;
        index_dirty.skip_racy_smudge_on_write = true;
        repo.write_index(&mut index_dirty)
            .expect("write dirty index");
        index_dirty.skip_racy_smudge_on_write = false;
        filetime::set_file_mtime(&index_path, pinned).expect("re-pin index");
        let index_probe = repo.load_index().expect("probe index");
        let index_mtime = crate::index::index_file_mtime(&index_path);
        let ie = index_probe.get(b"tracked.txt", 0).expect("entry");
        assert!(
            crate::diff::entry_is_racy_with_policy(
                ie,
                crate::diff::index_mtime_for_diff(&index_probe, index_mtime),
                crate::index::StatMatchPolicy::default(),
            ),
            "fixture must be racy relative to the on-disk index"
        );

        let git_status = std::process::Command::new("git")
            .args(["-C", &root.to_string_lossy(), "status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&git_status.stdout).contains("tracked.txt"),
            "git must see dirty tracked.txt"
        );

        assert_ne!(tree_a, tree_b, "branch trees must differ for this scenario");
        let tree_changes = diff_trees(repo.odb(), Some(&tree_a), Some(&tree_b), "").expect("diff");
        assert!(
            tree_changes
                .iter()
                .any(|c| c.old_path.as_deref() == Some("tracked.txt")),
            "tree diff must touch tracked.txt, got {tree_changes:?}"
        );
        let mut index_for_diff = repo.load_index().expect("index for diff");
        assert_eq!(
            index_for_diff.get(b"tracked.txt", 0).expect("ie").oid,
            oid_a,
            "index must still describe AAAA at switch time"
        );
        let (_, unstaged, _) = local_change_diffs_for_tree_changes(
            &repo,
            &mut index_for_diff,
            Some(tree_a),
            &tree_changes,
            None,
        )
        .expect("local diffs");
        assert!(
            !unstaged.is_empty(),
            "expected unstaged dirty tracked.txt before switch, got {unstaged:?}"
        );

        let switch_err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "other".into(),
                    oid: tip_b,
                },
                dry_run: false,
            },
        );
        assert!(
            switch_err.is_err(),
            "grit must refuse switch over same-size racy dirty content"
        );
        assert!(
            std::fs::read(&abs).expect("read").starts_with(b"DIRT"),
            "worktree must not be overwritten"
        );
    }

    #[test]
    fn local_change_diffs_for_tree_changes_uses_stat_only_when_racy_but_clean() {
        use crate::objects::{serialize_commit, CommitData, ObjectKind};
        use crate::refs;
        use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let repo = init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        std::fs::write(root.join("tracked.txt"), b"v1\n").expect("write");
        let mut index = repo.load_index().expect("index");
        let abs = root.join("tracked.txt");
        let oid = repo.odb().write(ObjectKind::Blob, b"v1\n").expect("blob");
        index.add_or_replace(
            crate::index::entry_from_stat(&abs, b"tracked.txt", oid, crate::index::MODE_REGULAR)
                .expect("entry"),
        );
        index.sort();
        let head_tree =
            write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                .expect("head tree");
        repo.write_index(&mut index).expect("write index");
        let index_path = repo.index_path_for_env().expect("index path");
        crate::index::advance_index_file_mtime_past_entries(&index, &index_path, None)
            .expect("advance index mtime");
        let commit = CommitData {
            tree: head_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "seed\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "HEAD", &co).expect("head");

        let mut target_index = index.clone();
        let oid2 = repo.odb().write(ObjectKind::Blob, b"v2\n").expect("blob");
        target_index.entries[0].oid = oid2;
        target_index.sort();
        let target_tree =
            write_tree_update_index(&repo.odb(), &mut target_index, "", WriteTreeFlags::silent())
                .expect("target tree");
        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");

        let mut index_probe = repo.load_index().expect("reload index");

        let (_, unstaged, index_changed) = local_change_diffs_for_tree_changes(
            &repo,
            &mut index_probe,
            Some(head_tree),
            &tree_changes,
            None,
        )
        .expect("scoped diffs");
        assert!(
            unstaged.is_empty(),
            "expected stat-only fast path, got {unstaged:?}"
        );
        assert!(!index_changed, "stat-only fast path must not rewrite index");
    }

    #[test]
    fn delete_only_tree_diff_paths_skip_worktree_stat_when_index_matches_head() {
        use crate::objects::{serialize_commit, CommitData, ObjectKind};
        use crate::refs;
        use crate::worktree_scan::{
            reset_worktree_metadata_probe, worktree_metadata_probe_count,
            worktree_read_dir_probe_count,
        };
        use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
        use std::thread;
        use std::time::Duration;

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let repo = init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        const FILE_COUNT: usize = 120;
        const DELETE_COUNT: usize = 80;
        const DIR_GROUPS: usize = 8;
        const FILES_PER_DIR: usize = DELETE_COUNT / DIR_GROUPS;
        for i in 0..FILE_COUNT {
            let dir = if i < DELETE_COUNT {
                format!("bulk/p{:02}", i / FILES_PER_DIR)
            } else {
                format!("keep/d{i:03}")
            };
            std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
            std::fs::write(
                root.join(&dir).join(format!("f{i:03}.txt")),
                format!("body {i}\n"),
            )
            .expect("write");
        }
        let mut index = repo.load_index().expect("index");
        for i in 0..FILE_COUNT {
            let rel = if i < DELETE_COUNT {
                format!("bulk/p{:02}/f{i:03}.txt", i / FILES_PER_DIR)
            } else {
                format!("keep/d{i:03}/f{i:03}.txt")
            };
            let abs = root.join(&rel);
            let data = std::fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let entry = crate::index::entry_from_stat(
                &abs,
                rel.as_bytes(),
                oid,
                crate::index::MODE_REGULAR,
            )
            .expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        let head_tree =
            write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                .expect("head tree");
        repo.write_index(&mut index).expect("write index");
        let commit = CommitData {
            tree: head_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "seed\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "HEAD", &co).expect("head");
        thread::sleep(Duration::from_secs(2));
        repo.write_index(&mut index).expect("refresh index mtime");

        let mut target_index = index.clone();
        for i in 0..DELETE_COUNT {
            let rel = format!("bulk/p{:02}/f{i:03}.txt", i / FILES_PER_DIR);
            target_index.remove(rel.as_bytes());
        }
        target_index.sort();
        let target_tree =
            write_tree_update_index(&repo.odb(), &mut target_index, "", WriteTreeFlags::silent())
                .expect("target tree");
        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");
        assert_eq!(
            tree_changes
                .iter()
                .filter(|c| c.status == DiffStatus::Deleted)
                .count(),
            DELETE_COUNT
        );

        let _probe = reset_worktree_metadata_probe(root);
        let mut index_probe = repo.load_index().expect("index");
        let (staged, unstaged, _) = local_change_diffs_for_tree_changes(
            &repo,
            &mut index_probe,
            Some(head_tree),
            &tree_changes,
            None,
        )
        .expect("scoped diffs");
        assert!(staged.is_empty(), "expected no staged: {staged:?}");
        assert!(unstaged.is_empty(), "clean tree: {unstaged:?}");
        let metadata = worktree_metadata_probe_count();
        let read_dirs = worktree_read_dir_probe_count();
        assert!(
            read_dirs <= DIR_GROUPS + 2,
            "expected directory-grouped scan ({DIR_GROUPS} groups), read_dir probes {read_dirs}"
        );
        assert!(
            metadata <= DELETE_COUNT + DIR_GROUPS + 4,
            "expected ~one metadata probe per deleted file via readdir, got {metadata}"
        );
    }

    #[test]
    fn switch_large_tree_diff_refuses_dirty_modified_path() {
        use crate::objects::{serialize_commit, CommitData, ObjectKind};
        use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
        use crate::refs;
        use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
        use std::thread;
        use std::time::Duration;

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let repo = init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        const FILE_COUNT: usize = 160;
        const MODIFY_COUNT: usize = 140;
        const DIRTY_REL: &str = "bulk/d000/f.txt";
        for i in 0..FILE_COUNT {
            let dir = format!("bulk/d{i:03}");
            std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
            std::fs::write(root.join(&dir).join("f.txt"), format!("body {i}\n")).expect("write");
        }
        let mut index = repo.load_index().expect("index");
        for i in 0..FILE_COUNT {
            let rel = format!("bulk/d{i:03}/f.txt");
            let abs = root.join(&rel);
            let data = std::fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let entry = crate::index::entry_from_stat(
                &abs,
                rel.as_bytes(),
                oid,
                crate::index::MODE_REGULAR,
            )
            .expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        let head_tree =
            write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                .expect("head tree");
        repo.write_index(&mut index).expect("write index");
        let head_commit = CommitData {
            tree: head_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "head\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let head_co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&head_commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "refs/heads/main", &head_co).expect("main");
        refs::write_symbolic_ref(&repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let mut target_index = index.clone();
        for i in 0..MODIFY_COUNT {
            let rel = format!("bulk/d{i:03}/f.txt");
            let abs = root.join(&rel);
            std::fs::write(&abs, format!("changed {i}\n")).expect("write touched");
            let data = std::fs::read(&abs).expect("read touched");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let entry = crate::index::entry_from_stat(
                &abs,
                rel.as_bytes(),
                oid,
                crate::index::MODE_REGULAR,
            )
            .expect("entry");
            target_index.add_or_replace(entry);
        }
        target_index.sort();
        let target_tree =
            write_tree_update_index(&repo.odb(), &mut target_index, "", WriteTreeFlags::silent())
                .expect("target tree");
        let target_commit = CommitData {
            tree: target_tree,
            parents: vec![head_co],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "target\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let target_co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&target_commit))
            .expect("target commit");
        refs::write_ref(&repo.git_dir(), "refs/heads/target", &target_co).expect("target");

        for i in 0..FILE_COUNT {
            let rel = format!("bulk/d{i:03}/f.txt");
            std::fs::write(root.join(&rel), format!("body {i}\n")).expect("restore worktree");
        }
        let mut index_on_main = repo.load_index().expect("index on main");
        for i in 0..FILE_COUNT {
            let rel = format!("bulk/d{i:03}/f.txt");
            let abs = root.join(&rel);
            let data = std::fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let entry = crate::index::entry_from_stat(
                &abs,
                rel.as_bytes(),
                oid,
                crate::index::MODE_REGULAR,
            )
            .expect("entry");
            index_on_main.add_or_replace(entry);
        }
        index_on_main.sort();
        repo.write_index(&mut index_on_main)
            .expect("sync index to main worktree");

        thread::sleep(Duration::from_secs(2));
        repo.write_index(&mut index_on_main)
            .expect("advance index mtime past entry mtimes");

        let dirty_bytes = b"unsaved edit after non-racy index\n";
        std::fs::write(root.join(DIRTY_REL), dirty_bytes).expect("dirty write");
        let index_path = repo.index_path_for_env().expect("index path");
        let index_probe = repo.load_index().expect("probe index");
        let index_mtime = crate::diff::index_mtime_for_diff(
            &index_probe,
            crate::index::index_file_mtime(&index_path),
        );
        let ie = index_probe.get(DIRTY_REL.as_bytes(), 0).expect("dirty ie");
        assert!(
            !crate::diff::entry_is_racy_with_policy(
                ie,
                index_mtime,
                crate::index::StatMatchPolicy::default(),
            ),
            "fixture must use non-racy index entry with stale content"
        );

        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");
        assert!(
            tree_changes.len() >= MODIFY_COUNT,
            "expected large modified tree diff, got {}",
            tree_changes.len()
        );

        let mut index_for_diff = repo.load_index().expect("index for diff");
        let (_, unstaged, _) = local_change_diffs_for_tree_changes(
            &repo,
            &mut index_for_diff,
            Some(head_tree),
            &tree_changes,
            None,
        )
        .expect("local diffs");
        assert!(
            unstaged
                .iter()
                .any(|e| e.old_path.as_deref() == Some(DIRTY_REL)),
            "expected unstaged dirty {DIRTY_REL}, got {unstaged:?}"
        );

        let switch_err = switch_repository(
            &repo,
            &SwitchOptions {
                mode: SwitchMode::Existing {
                    branch: "target".into(),
                    oid: target_co,
                },
                dry_run: false,
            },
        );
        assert!(
            switch_err.is_err(),
            "grit must refuse switch with local edits across a large tree diff"
        );
        assert_eq!(
            std::fs::read(root.join(DIRTY_REL)).expect("read dirty"),
            dirty_bytes,
            "worktree edit must survive refused switch"
        );
    }

    #[test]
    fn local_change_diffs_for_tree_changes_limits_worktree_probes() {
        use crate::objects::{serialize_commit, CommitData, ObjectKind};
        use crate::refs;
        use crate::worktree_scan::{reset_worktree_metadata_probe, worktree_metadata_probe_count};
        use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let repo = init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        const FILE_COUNT: usize = 800;
        const TOUCH: usize = 40;
        for i in 0..FILE_COUNT {
            let dir = format!("d{:03}", i % 20);
            std::fs::create_dir_all(root.join(&dir)).expect("mkdir");
            std::fs::write(
                root.join(&dir).join(format!("f{i:04}.txt")),
                format!("body {i}\n"),
            )
            .expect("write");
        }
        let mut index = repo.load_index().expect("index");
        for i in 0..FILE_COUNT {
            let rel = format!("d{:03}/f{i:04}.txt", i % 20);
            let abs = root.join(&rel);
            let data = std::fs::read(&abs).expect("read");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let entry = crate::index::entry_from_stat(
                &abs,
                rel.as_bytes(),
                oid,
                crate::index::MODE_REGULAR,
            )
            .expect("entry");
            index.add_or_replace(entry);
        }
        index.sort();
        let head_tree =
            write_tree_update_index(&repo.odb(), &mut index, "", WriteTreeFlags::silent())
                .expect("head tree");
        repo.write_index(&mut index).expect("write index");
        let commit = CommitData {
            tree: head_tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "seed\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(ObjectKind::Commit, &serialize_commit(&commit))
            .expect("commit");
        refs::write_ref(&repo.git_dir(), "HEAD", &co).expect("head");

        let mut target_index = index.clone();
        for i in 0..TOUCH {
            let rel = format!("d{:03}/f{i:04}.txt", i % 20);
            let abs = root.join(&rel);
            std::fs::write(&abs, format!("changed {i}\n")).expect("write touched");
            let data = std::fs::read(&abs).expect("read touched");
            let oid = repo.odb().write(ObjectKind::Blob, &data).expect("blob");
            let entry = crate::index::entry_from_stat(
                &abs,
                rel.as_bytes(),
                oid,
                crate::index::MODE_REGULAR,
            )
            .expect("entry");
            target_index.add_or_replace(entry);
        }
        target_index.sort();
        let target_tree =
            write_tree_update_index(&repo.odb(), &mut target_index, "", WriteTreeFlags::silent())
                .expect("target tree");
        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");
        assert!(
            tree_changes.len() >= TOUCH,
            "expected at least {TOUCH} tree changes, got {}",
            tree_changes.len()
        );

        std::fs::write(root.join("d000/f9999.txt"), b"off-tree dirty\n").expect("dirty off-tree");

        let _probe = reset_worktree_metadata_probe(root);
        let mut index_probe = repo.load_index().expect("index");
        let _ = local_change_diffs_for_tree_changes(
            &repo,
            &mut index_probe,
            Some(head_tree),
            &tree_changes,
            None,
        )
        .expect("scoped diffs");
        let calls = worktree_metadata_probe_count();
        assert!(
            calls <= TOUCH * 3 + 24,
            "scoped switch diff must not scan all {FILE_COUNT} tracked paths (metadata probes {calls})"
        );
    }

    #[test]
    fn ensure_worktree_clean_for_pick_passes_on_fresh_commit() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        let mut index = repo.load_index().expect("index");
        let data = std::fs::read(wt.join("tracked.txt")).expect("read");
        let oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, &data)
            .expect("blob");
        let entry = crate::index::entry_from_stat(
            &wt.join("tracked.txt"),
            b"tracked.txt",
            oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        repo.write_index(&mut index).expect("write index");
        let tree = crate::write_tree::write_tree_update_index(
            &repo.odb(),
            &mut index,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .expect("tree");
        let commit = crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "init\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(
                crate::objects::ObjectKind::Commit,
                &crate::objects::serialize_commit(&commit),
            )
            .expect("commit");
        crate::refs::write_ref(&repo.git_dir(), "HEAD", &co).expect("head");
        ensure_worktree_clean_for_pick(&repo).expect("clean");
    }

    #[test]
    fn count_indexed_paths_under_root_ignores_lexicographic_sibling_prefixes() {
        use crate::index::{Index, IndexEntry, MODE_REGULAR};
        let mut index = Index::new();
        for path in ["dir-old/f.txt", "dir/f.txt", "dir/nested/g.txt"] {
            index.add_or_replace(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: MODE_REGULAR,
                uid: 0,
                gid: 0,
                size: 0,
                oid: ObjectId::zero(),
                flags: path.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path.as_bytes().to_vec(),
                base_index_pos: 0,
            });
        }
        index.sort();
        assert_eq!(count_indexed_paths_under_root(&index, "dir"), 2);
        assert_eq!(count_indexed_paths_under_root(&index, "dir-old"), 1);
    }

    #[test]
    fn scan_wholesale_delete_root_walks_directory_marked_for_delete() {
        use crate::index::entry_from_stat;
        use crate::objects::ObjectKind;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        let root = "wh";
        std::fs::create_dir_all(wt.join(root).join("nested")).expect("mkdir");
        let mut delete_paths = HashSet::new();
        let mut index = repo.load_index().expect("index");
        for i in 0..31 {
            let rel = format!("{root}/f{i:02}.txt");
            let abs = wt.join(&rel);
            std::fs::write(&abs, format!("v{i}\n")).expect("write");
            let oid = repo
                .odb()
                .write(ObjectKind::Blob, format!("v{i}\n").as_bytes())
                .expect("blob");
            index.add_or_replace(
                entry_from_stat(&abs, rel.as_bytes(), oid, crate::index::MODE_REGULAR)
                    .expect("entry"),
            );
            if i < 30 {
                delete_paths.insert(rel);
            }
        }
        std::fs::create_dir_all(wt.join(format!("{root}/nested"))).expect("mkdir nested");
        delete_paths.insert(format!("{root}/nested"));
        let fifo = wt.join(format!("{root}/nested/block.pipe"));
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success(), "mkfifo must succeed on unix");
        index.sort();
        let indexed = count_indexed_paths_under_root(&index, root);
        let deletes = count_delete_paths_under_root(&delete_paths, root, false);
        assert_eq!(indexed, deletes, "fixture must be index-aligned");
        let scan = scan_wholesale_delete_root(&index, wt, root, &delete_paths, false, None);
        assert!(
            scan.untracked.iter().any(|p| p == "wh/nested/block.pipe"),
            "special files under a scheduled-delete directory must be discovered"
        );
    }
}
