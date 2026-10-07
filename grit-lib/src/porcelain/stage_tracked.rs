//! Stage modifications and deletions of already-tracked paths (`git commit -a` / `-a` staging).
//!
//! Unlike a full `git add`, untracked paths are ignored. Tracked paths whose worktree stat tuple
//! and mode match the index (and are not racy relative to the index file mtime) are trusted without
//! re-reading file contents — Git's `ie_match_stat` fast path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::{
    entry_is_racy, index_entry_worktree_abs, mode_from_metadata, read_submodule_head_oid,
    stat_matches, symlink_target_bytes, SymlinkDirCache,
};
use crate::error::{Error, Result};
use crate::index::index_file_mtime;
use crate::index::{
    entry_from_stat, worktree_path_from_index_rel, Index, IndexEntry, MODE_GITLINK,
};
use crate::objects::{ObjectId, ObjectKind};
use crate::precompose_config::effective_core_precomposeunicode_with_config;
use crate::repo::Repository;
use crate::unicode_normalization::resolve_worktree_path_for_staging;
use crate::worktree_scan::{
    for_each_blob_by_directory_parallel, group_blob_entries_by_dir, BlobDiskLookup,
    WorktreeBlobScanOptions,
};

/// Summary of paths updated while staging tracked modifications/deletions.
///
/// Paths are stored as raw index bytes (may be non-UTF-8 on Unix).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageTrackedSummary {
    /// Tracked paths newly staged or re-staged with updated blob/mode metadata.
    pub modified: Vec<Vec<u8>>,
    /// Tracked paths removed from the index because they disappeared from the worktree.
    pub removed: Vec<Vec<u8>>,
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
/// # Parameters
/// - `repo` — repository handle (object store and index paths).
/// - `work_tree` — working tree root.
///
/// # Returns
/// A [`StageTrackedSummary`] listing modified and removed index paths (sorted).
///
/// # Errors
/// I/O failures, object write failures, or a missing index when one is required.
pub fn stage_tracked_modifications(
    repo: &Repository,
    work_tree: &Path,
) -> Result<StageTrackedSummary> {
    let index_path = repo.index_path_for_env()?;
    let mut index = match repo.load_index_at(&index_path) {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StageTrackedSummary::default());
        }
        Err(e) => return Err(e),
    };

    let summary = stage_tracked_modifications_in_index(repo, work_tree, &index_path, &mut index)?;

    if !summary.modified.is_empty() || !summary.removed.is_empty() {
        repo.write_index_at(&index_path, &mut index)?;
    }

    Ok(summary)
}

/// Like [`stage_tracked_modifications`], but mutates a caller-owned index without writing it.
///
/// Useful when the caller will merge further index updates before a single write (for example
/// `git commit --dry-run`).
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
    let index_mtime = index_file_mtime(index_path);
    let precompose_unicode = effective_core_precomposeunicode_with_config(
        Some(&repo.git_dir),
        repo.config().ok().as_deref(),
    );
    let trust_filemode = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_bool("core.filemode").and_then(|r| r.ok()))
        .unwrap_or(true);

    let mut unmerged_paths: HashSet<Vec<u8>> = HashSet::new();
    let mut stage0: HashMap<Vec<u8>, IndexEntry> = HashMap::new();
    for e in &index.entries {
        if e.stage() != 0 {
            unmerged_paths.insert(e.path.clone());
        } else {
            stage0.insert(e.path.clone(), e.clone());
        }
    }

    let mut symlink_parent_cache: HashMap<PathBuf, bool> = HashMap::new();
    let mut summary = StageTrackedSummary::default();
    let ignorecase = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);
    let mut dir_symlinks = SymlinkDirCache::default();

    for raw_path in &unmerged_paths {
        let abs_path = abs_path_for_stage_tracked(work_tree, raw_path, precompose_unicode);
        refresh_unmerged_tracked_path(repo, index, raw_path, &abs_path, &mut summary)?;
    }

    for (raw_path, idx_e) in &stage0 {
        if idx_e.mode == MODE_GITLINK || std::str::from_utf8(raw_path).is_err() {
            stage_tracked_path_individual(
                repo,
                work_tree,
                index,
                raw_path,
                idx_e,
                index_mtime,
                trust_filemode,
                precompose_unicode,
                &mut symlink_parent_cache,
                &mut summary,
            )?;
        }
    }

    let blob_dirs = group_blob_entries_by_dir(index);
    let work_tree_owned = work_tree.to_path_buf();
    let dir_abs = move |dir: &str| {
        if dir.is_empty() {
            work_tree_owned.clone()
        } else {
            index_entry_worktree_abs(&work_tree_owned, dir, precompose_unicode, ignorecase)
        }
    };
    let work_tree_for_files = work_tree.to_path_buf();
    let work_tree_for_symlink = work_tree.to_path_buf();
    let file_abs = move |rel: &str| {
        index_entry_worktree_abs(&work_tree_for_files, rel, precompose_unicode, ignorecase)
    };
    let preload_index = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_bool("core.preloadIndex").and_then(|r| r.ok()))
        .unwrap_or(true);
    let blob_scan_opts = WorktreeBlobScanOptions {
        preload_index,
        stat_parallel_threads: None,
    };
    let mut blob_scan: Vec<(String, BlobDiskLookup)> = Vec::new();
    for_each_blob_by_directory_parallel(
        &blob_dirs,
        blob_scan_opts,
        dir_abs,
        file_abs,
        |rel_path| {
            dir_symlinks.has_symlink_in_path(work_tree, rel_path, precompose_unicode, ignorecase)
                || {
                    let abs = index_entry_worktree_abs(
                        &work_tree_for_symlink,
                        rel_path,
                        precompose_unicode,
                        ignorecase,
                    );
                    path_has_symlink_parent_cached(work_tree, &abs, &mut symlink_parent_cache)
                }
        },
        |_entry_index, rel_path, lookup| {
            blob_scan.push((rel_path.to_owned(), lookup));
            Ok(())
        },
    )?;

    for (rel_path, lookup) in blob_scan {
        let Some(idx_e) = index.get(rel_path.as_bytes(), 0).cloned() else {
            continue;
        };
        let raw_path = idx_e.path.clone();
        let abs_path = abs_path_for_stage_tracked(work_tree, &raw_path, precompose_unicode);
        match lookup {
            BlobDiskLookup::Missing => {
                if idx_e.skip_worktree() {
                    continue;
                }
                if rel_path_has_symlink_parent(work_tree, &rel_path) {
                    if index.remove(&raw_path) {
                        summary.removed.push(raw_path);
                    }
                    continue;
                }
                if symlink_metadata_for_staging(&abs_path)?.is_none() && index.remove(&raw_path) {
                    summary.removed.push(raw_path);
                }
            }
            BlobDiskLookup::Io(e) => return Err(Error::Io(e)),
            BlobDiskLookup::Present(meta) => {
                let ctx = PresentRefreshCtx {
                    repo,
                    index,
                    raw_path: &raw_path,
                    abs_path: &abs_path,
                    idx_e: &idx_e,
                    index_mtime,
                    trust_filemode,
                };
                if refresh_present_tracked_path(ctx, &meta)? {
                    summary.modified.push(raw_path);
                }
            }
        }
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
    trust_filemode: bool,
}

fn refresh_unmerged_tracked_path(
    repo: &Repository,
    index: &mut Index,
    raw_path: &[u8],
    abs_path: &Path,
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
    match symlink_metadata_for_staging(abs_path)? {
        Some(_) => {
            index.remove(raw_path);
            if idx_mode == 0o160000 {
                if let Some(oid) = read_submodule_head_oid(abs_path) {
                    stage_gitlink_from_stat(abs_path, raw_path, oid, index)?;
                    summary.modified.push(raw_path.to_vec());
                }
            } else {
                stage_blob_from_worktree(repo, index, abs_path, raw_path, None)?;
                summary.modified.push(raw_path.to_vec());
            }
        }
        None => {
            index.remove(raw_path);
            summary.removed.push(raw_path.to_vec());
        }
    }
    Ok(())
}

/// Worktree metadata for staging: present, definitively missing, or I/O error.
fn symlink_metadata_for_staging(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if worktree_path_is_missing(&e) => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

fn worktree_path_is_missing(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::NotFound)
}

fn abs_path_for_stage_tracked(
    work_tree: &Path,
    raw_path: &[u8],
    precompose_unicode: bool,
) -> PathBuf {
    std::str::from_utf8(raw_path)
        .ok()
        .filter(|_| precompose_unicode)
        .map_or_else(
            || worktree_path_from_index_rel(work_tree, raw_path),
            |rel| resolve_worktree_path_for_staging(work_tree, rel, true).abs,
        )
}

#[allow(clippy::too_many_arguments)]
fn stage_tracked_path_individual(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    raw_path: &[u8],
    idx_e: &IndexEntry,
    index_mtime: Option<(u32, u32)>,
    trust_filemode: bool,
    precompose_unicode: bool,
    symlink_parent_cache: &mut HashMap<PathBuf, bool>,
    summary: &mut StageTrackedSummary,
) -> Result<()> {
    let abs_path = abs_path_for_stage_tracked(work_tree, raw_path, precompose_unicode);
    if path_has_symlink_parent_cached(work_tree, &abs_path, symlink_parent_cache) {
        if index.remove(raw_path) {
            summary.removed.push(raw_path.to_vec());
        }
        return Ok(());
    }
    match symlink_metadata_for_staging(&abs_path)? {
        Some(meta) => {
            let ctx = PresentRefreshCtx {
                repo,
                index,
                raw_path,
                abs_path: &abs_path,
                idx_e,
                index_mtime,
                trust_filemode,
            };
            if refresh_present_tracked_path(ctx, &meta)? {
                summary.modified.push(raw_path.to_vec());
            }
        }
        None if idx_e.skip_worktree() => {}
        None => {
            if index.remove(raw_path) {
                summary.removed.push(raw_path.to_vec());
            }
        }
    }
    Ok(())
}

fn refresh_present_tracked_path(ctx: PresentRefreshCtx<'_>, meta: &fs::Metadata) -> Result<bool> {
    let PresentRefreshCtx {
        repo,
        index,
        raw_path,
        abs_path,
        idx_e,
        index_mtime,
        trust_filemode,
    } = ctx;
    let idx_mode = idx_e.mode;
    let idx_intent_to_add = idx_e.intent_to_add();
    let idx_oid = idx_e.oid;

    if idx_mode == 0o160000 {
        return refresh_gitlink(repo, index, raw_path, abs_path, idx_e);
    }

    if meta.is_dir() && !meta.file_type().is_symlink() {
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

    let wt_mode = mode_from_metadata(meta);
    let stat_same = stat_matches(idx_e, meta);
    let racy = entry_is_racy(idx_e, index_mtime);
    if !idx_intent_to_add && idx_e.size != 0 && stat_same && !racy {
        if wt_mode == idx_mode || !trust_filemode {
            return Ok(false);
        }
        let entry = entry_from_stat(abs_path, raw_path, idx_oid, wt_mode)?;
        index.stage_file(entry);
        return Ok(true);
    }

    let oid = read_worktree_blob_oid(repo, abs_path, meta)?;
    let mode_for_index = if trust_filemode { wt_mode } else { idx_mode };
    if !idx_intent_to_add && idx_oid == oid && (wt_mode == idx_mode || !trust_filemode) {
        return Ok(false);
    }
    let entry = entry_from_stat(abs_path, raw_path, oid, mode_for_index)?;
    index.stage_file(entry);
    Ok(true)
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
) -> Result<()> {
    let meta = fs::symlink_metadata(abs_path)?;
    let oid = read_worktree_blob_oid(repo, abs_path, &meta)?;
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
) -> Result<ObjectId> {
    let data = if meta.file_type().is_symlink() {
        let target = fs::read_link(abs_path)?;
        symlink_target_bytes(&target)
    } else {
        #[cfg(test)]
        repo.odb.hot_path_test_metrics().record_blob_content_read();
        fs::read(abs_path)?
    };
    repo.odb.write(ObjectKind::Blob, &data)
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

    use crate::diff::{entry_is_racy, stat_matches};
    use crate::index::{
        entry_from_metadata, index_file_mtime, Index, MODE_EXECUTABLE, MODE_REGULAR,
    };
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use crate::repo::{init_repository, Repository};
    use tempfile::TempDir;

    use super::*;

    const INDEX_MTIME: (u32, u32) = (1_700_000_000, 123_456_789);

    fn init_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = init_repository(dir.path(), false, "main", None, "files").unwrap();
        (dir, repo)
    }

    #[test]
    fn stage_tracked_after_deleting_first_of_three_paths() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"a.txt", b"a\n");
        write_and_index(&repo, b"b.txt", b"b\n");
        write_and_index(&repo, b"c.txt", b"c\n");
        finalize_index_for_stat_trust(&repo);

        let wt = repo.work_tree.as_ref().unwrap();
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
        let wt = repo.work_tree.as_ref().unwrap();
        const FILE_COUNT: usize = 2000;
        for i in 0..FILE_COUNT {
            let rel = format!("d{:04}/f{:05}.txt", i / 100, i);
            write_and_index(&repo, rel.as_bytes(), b"x\n");
        }
        reset_worktree_metadata_probe();
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
        let wt = repo.work_tree.as_ref().unwrap();
        let abs = worktree_path_from_index_rel(wt, rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, content).unwrap();
        let oid = repo.odb.write(ObjectKind::Blob, content).unwrap();
        let mut index = repo.load_index().unwrap();
        let meta = fs::symlink_metadata(&abs).unwrap();
        let mode = mode_from_metadata(&meta);
        let entry = entry_from_stat(&abs, rel, oid, mode).unwrap();
        index.add_or_replace(entry);
        repo.write_index(&mut index).unwrap();
    }

    fn finalize_index_for_stat_trust(repo: &Repository) {
        let wt = repo.work_tree.as_ref().unwrap();
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
            worktree_path_from_index_rel(repo.work_tree.as_ref().unwrap(), b"b.txt"),
            b"two-changed",
        )
        .unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree.as_ref().unwrap()).unwrap();
        assert_eq!(summary.modified, vec![b"b.txt".to_vec()]);
        assert!(summary.removed.is_empty());

        let index = repo.load_index().unwrap();
        let b = index.entries.iter().find(|e| e.path == b"b.txt").unwrap();
        let expected = repo.odb.write(ObjectKind::Blob, b"two-changed").unwrap();
        assert_eq!(b.oid, expected);
    }

    #[test]
    fn removes_deleted_tracked_file() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"gone.txt", b"x");
        fs::remove_file(worktree_path_from_index_rel(
            repo.work_tree.as_ref().unwrap(),
            b"gone.txt",
        ))
        .unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree.as_ref().unwrap()).unwrap();
        assert_eq!(summary.removed, vec![b"gone.txt".to_vec()]);
        assert!(repo.load_index().unwrap().entries.is_empty());
    }

    #[test]
    fn unchanged_tracked_file_is_not_rehashed() {
        let (_dir, repo) = init_repo();
        write_and_index(&repo, b"clean.txt", b"same");
        finalize_index_for_stat_trust(&repo);

        let objects_before = count_loose_objects(&repo);
        let summary = stage_tracked_modifications(&repo, repo.work_tree.as_ref().unwrap()).unwrap();
        let objects_after = count_loose_objects(&repo);

        assert!(summary.modified.is_empty());
        assert!(summary.removed.is_empty());
        assert_eq!(objects_before, objects_after);
    }

    fn count_loose_objects(repo: &Repository) -> usize {
        let objects = repo.git_dir.join("objects");
        let mut count = 0usize;
        if let Ok(entries) = fs::read_dir(objects) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.len() == 2 {
                    if let Ok(sub) = fs::read_dir(entry.path()) {
                        count += sub.count();
                    }
                }
            }
        }
        count
    }

    #[test]
    fn racy_same_size_modification_is_detected() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree.as_ref().unwrap();
        let content_a = b"12345";
        let content_b = b"54321";
        assert_eq!(content_a.len(), content_b.len());

        write_and_index(&repo, b"racy.txt", content_a);
        let file_path = worktree_path_from_index_rel(wt, b"racy.txt");
        fs::write(&file_path, content_b).unwrap();
        pin_mtime(&file_path, INDEX_MTIME.0, INDEX_MTIME.1);

        let meta = fs::symlink_metadata(&file_path).unwrap();
        let stale_oid = repo.odb.hash(ObjectKind::Blob, content_a);
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
        let expected = repo.odb.write(ObjectKind::Blob, content_b).unwrap();
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
        let wt = repo.work_tree.as_ref().unwrap();
        let content_a = b"aaaaa";
        let content_b = b"bbbbb";
        assert_eq!(content_a.len(), content_b.len());

        let file_path = worktree_path_from_index_rel(wt, b"f.txt");
        fs::write(&file_path, content_b).unwrap();
        pin_mtime(&file_path, INDEX_MTIME.0, INDEX_MTIME.1);
        let meta = fs::symlink_metadata(&file_path).unwrap();
        let stale_oid = repo.odb.hash(ObjectKind::Blob, content_a);

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
        let path = worktree_path_from_index_rel(repo.work_tree.as_ref().unwrap(), b"run.sh");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o100755);
        fs::set_permissions(&path, perms).unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree.as_ref().unwrap()).unwrap();
        assert_eq!(summary.modified, vec![b"run.sh".to_vec()]);
        let index = repo.load_index().unwrap();
        let e = index.entries.iter().find(|e| e.path == b"run.sh").unwrap();
        assert_eq!(e.mode, MODE_EXECUTABLE);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_under_symlinked_parent() {
        let dir = TempDir::new().unwrap();
        let repo = init_repository(dir.path(), false, "main", None, "files").unwrap();
        let wt = repo.work_tree.as_ref().unwrap();
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
            worktree_path_from_index_rel(repo.work_tree.as_ref().unwrap(), b"new.txt"),
            b"new",
        )
        .unwrap();

        let summary = stage_tracked_modifications(&repo, repo.work_tree.as_ref().unwrap()).unwrap();
        assert!(summary.modified.is_empty());
        assert!(summary.removed.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_parent_does_not_stage_deletion() {
        use std::io::ErrorKind;
        use std::os::unix::fs::PermissionsExt;

        let (_dir, repo) = init_repo();
        let wt = repo.work_tree.as_ref().unwrap();
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
            .open(repo.git_dir.join("config"))
            .unwrap()
            .write_all(b"\n[core]\n\tprecomposeunicode = true\n")
            .unwrap();

        let wt = repo.work_tree.as_ref().unwrap();
        let nfd = "cafe\u{0301}.txt";
        let nfc = "caf\u{00e9}.txt";
        let abs = wt.join(nfd);
        fs::write(&abs, b"one").unwrap();
        let oid = repo.odb.write(ObjectKind::Blob, b"one").unwrap();
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
        let expected = repo.odb.write(ObjectKind::Blob, b"changed").unwrap();
        assert_eq!(repo.load_index().unwrap().entries[0].oid, expected);
    }

    #[test]
    fn filemode_false_skips_executable_bit_only_refresh() {
        let dir = TempDir::new().unwrap();
        let repo = init_repository(dir.path(), false, "main", None, "files").unwrap();
        fs::write(
            repo.git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = false\n\tbare = false\n",
        )
        .unwrap();
        fs::write(dir.path().join("run.sh"), b"x\n").unwrap();
        let git_dir = repo.git_dir.clone();
        let wt = repo.work_tree.as_ref().unwrap();
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
        let wt = repo.work_tree.as_ref().unwrap();
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
}
