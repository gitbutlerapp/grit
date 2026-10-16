//! Lightweight worktree cleanliness checks without a full [`status`](super::status::status) pass.
//!
//! Pick, merge, and branch switch only need to know whether staged or unstaged changes exist
//! (and switch additionally needs untracked paths plus a tree-to-tree diff). This module loads
//! the index once, refreshes stat data when needed, and computes those slices without rename
//! detection, ahead/behind, stash counts, or in-progress operation state.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::ConfigSet;
use crate::diff::format_mode;
use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, entry_is_racy_with_policy,
    index_mtime_for_diff, DiffEntry, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::StatMatchPolicy;
use crate::index::{index_file_mtime, Index, IndexEntry};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::checkout::tree_changes_touch_gitattributes;
use crate::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
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

    let index_path = repo.index_path_for_env()?;
    let mut index = repo.load_index_at(&index_path)?;
    let _ = index.expand_sparse_directory_placeholders(repo.odb());

    let head = resolve_head(repo.git_dir())?;
    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb().read(oid)?;
            Some(parse_commit(&obj.data)?.tree)
        }
        None => None,
    };

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
) -> Result<(Vec<DiffEntry>, Vec<DiffEntry>, bool)> {
    let _ = head_tree;
    if index.entries.iter().any(|e| e.stage() != 0) {
        return local_change_diffs(repo, index, head_tree);
    }
    let paths = paths_touched_by_tree_changes(tree_changes);
    if paths.is_empty() {
        return Ok((Vec::new(), Vec::new(), false));
    }
    let path_filter = Arc::new(paths);
    let head_by_path = head_tree_sides_from_changes(tree_changes);
    let staged = staged_diff_for_tree_switch_paths(index, &head_by_path, path_filter.as_ref())?;

    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path_for_env()?;
    let index_mtime = index_mtime_for_diff(index, index_file_mtime(&index_path));
    let stat_policy = StatMatchPolicy::from_config(repo.config().ok().as_deref());
    if staged.is_empty()
        && !touched_paths_include_gitlinks(index, path_filter.as_ref())
        && !touched_paths_include_racy_index_entries(
            index,
            path_filter.as_ref(),
            index_mtime,
            stat_policy,
        )
        && worktree_stats_match_index_on_paths(
            repo,
            index,
            path_filter.as_ref(),
            index_mtime,
            stat_policy,
        )?
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
    let (unstaged, index_changed) = diff_index_to_worktree_with_options(
        repo.odb(),
        index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            index_mtime,
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            refresh_index_stat_in_pass: false,
            for_status: false,
            stat_parallel_threads: None,
            config,
            worktree_rules,
            restrict_to_paths: Some(path_filter),
            ..Default::default()
        },
    )?;
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

/// True when a touched path has a racy index timestamp relative to the on-disk index file.
fn touched_paths_include_racy_index_entries(
    index: &Index,
    paths: &BTreeSet<String>,
    index_mtime: Option<(u32, u32)>,
    stat_policy: StatMatchPolicy,
) -> bool {
    paths.iter().any(|path| {
        index.get(path.as_bytes(), 0).is_some_and(|ie| {
            indexed_path_needs_worktree_stat_check(ie)
                && entry_is_racy_with_policy(ie, index_mtime, stat_policy)
        })
    })
}

/// Whether every indexed blob under `paths` matches worktree stat data (no content read).
///
/// Callers must treat racy entries as non-matching (see [`touched_paths_include_racy_index_entries`])
/// and fall back to a content-aware diff.
fn worktree_stats_match_index_on_paths(
    repo: &Repository,
    index: &Index,
    paths: &BTreeSet<String>,
    index_mtime: Option<(u32, u32)>,
    stat_policy: StatMatchPolicy,
) -> Result<bool> {
    use crate::diff::index_entry_worktree_abs;
    use crate::diff::stat_matches_with_policy;
    use crate::index::MODE_GITLINK;
    use crate::parallel::ParallelOp;
    use crate::worktree_scan::{
        for_each_blob_by_directory_parallel, group_blob_entries_by_dir_for_paths, BlobDiskLookup,
        WorktreeBlobScanOptions,
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
    let blob_dirs = group_blob_entries_by_dir_for_paths(index, paths);
    if blob_dirs.is_empty() {
        return Ok(true);
    }
    let preload_index = config
        .as_ref()
        .and_then(|c| c.get_bool("core.preloadIndex").and_then(|r| r.ok()))
        .unwrap_or(true);
    let stat_parallel_threads = (paths.len() >= 48).then(|| {
        repo.thread_policy()
            .resolve(
                ParallelOp::IndexRefresh,
                config.as_deref().unwrap_or(&ConfigSet::new()),
            )
            .threads
            .get()
    });
    let blob_scan = WorktreeBlobScanOptions {
        preload_index,
        stat_parallel_threads,
    };
    let work_tree_owned = work_tree.to_path_buf();
    let dir_abs = move |dir: &str| {
        if dir.is_empty() {
            work_tree_owned.clone()
        } else {
            index_entry_worktree_abs(&work_tree_owned, dir, precompose_unicode, ignorecase)
        }
    };
    let work_tree_for_files = work_tree.to_path_buf();
    let file_abs = move |rel: &str| {
        index_entry_worktree_abs(&work_tree_for_files, rel, precompose_unicode, ignorecase)
    };
    let matches = AtomicBool::new(true);
    for_each_blob_by_directory_parallel(
        &blob_dirs,
        blob_scan,
        &Default::default(),
        config.as_deref().unwrap_or(&ConfigSet::new()),
        false,
        dir_abs,
        file_abs,
        |_| false,
        |entry_index, _path, lookup| {
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
                BlobDiskLookup::Present(meta) => {
                    let mismatch = if ie.mode == MODE_GITLINK {
                        !meta.is_dir()
                    } else if !meta.is_file() && !meta.is_symlink() {
                        true
                    } else {
                        !stat_matches_with_policy(ie, &meta, stat_policy)
                            || entry_is_racy_with_policy(ie, index_mtime, stat_policy)
                    };
                    if mismatch {
                        matches.store(false, Ordering::Relaxed);
                    }
                }
            }
            Ok(())
        },
    )?;
    Ok(matches.load(Ordering::Relaxed))
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

fn paths_touched_by_tree_changes(tree_changes: &[DiffEntry]) -> BTreeSet<String> {
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
) -> Result<(Vec<String>, Vec<String>, bool, bool)> {
    let added_specs = untracked_scan_pathspecs_for_tree_changes(tree_changes);
    let delete_specs = untracked_scan_pathspecs_for_deletes(tree_changes);
    let delete_roots: BTreeSet<String> = delete_specs.iter().cloned().collect();
    let mut pathspecs: BTreeSet<String> = added_specs.iter().cloned().collect();
    pathspecs.extend(delete_specs);
    if pathspecs.is_empty() {
        return Ok((Vec::new(), Vec::new(), true, false));
    }
    let specs: Vec<String> = pathspecs.into_iter().collect();
    let need_traditional_ignored = !delete_roots.is_empty();
    let ignored_mode = if need_traditional_ignored {
        IgnoredMode::Traditional
    } else {
        IgnoredMode::No
    };
    let ignorecase = repo
        .config()
        .ok()
        .and_then(|c| c.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);
    let (untracked, ignored) =
        collect_untracked_and_ignored(repo, index, work_tree, ignored_mode, true, &specs)?;
    let trust_wholesale = delete_roots.is_empty() || {
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
        !untracked.iter().any(|p| under_delete_root(p))
            && !ignored.iter().any(|p| under_delete_root(p))
    };
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
/// Does not check for staged or unstaged changes; callers such as `grit merge` enforce a clean
/// worktree separately.
///
/// # Errors
///
/// Returns [`Error::Message`] when an untracked path would be overwritten, or on I/O/ODB failures.
pub fn prepare_tree_checkout(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let tree_changes = diff_trees(repo.odb(), from_tree, Some(to_tree), "")?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, &tree_changes)?;
    ensure_no_untracked_overwrite(&untracked, &tree_changes)
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
        let (scoped_staged, scoped_unstaged, _) =
            local_change_diffs_for_tree_changes(&repo, &mut index4, Some(head_tree), &tree_changes)
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
}
