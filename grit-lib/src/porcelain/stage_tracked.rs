//! Stage modifications and deletions of already-tracked paths (`git commit -a` / `-a` staging).
//!
//! Unlike a full `git add`, untracked paths are ignored. Tracked paths whose worktree stat tuple
//! and mode match the index (and are not racy relative to the index file mtime) are trusted without
//! re-reading file contents — Git's `ie_match_stat` fast path.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::{
    entry_is_racy, mode_from_metadata, read_submodule_head_oid, stat_matches, symlink_target_bytes,
};
use crate::error::{Error, Result};
use crate::index::index_file_mtime;
use crate::index::{entry_from_stat, worktree_path_from_index_rel, Index, IndexEntry};
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;

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
/// Untracked files are not added. [`skip_worktree`] entries are left unchanged; missing paths
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

    let mut path_keys: HashSet<Vec<u8>> = HashSet::new();
    let mut unmerged_paths: HashSet<Vec<u8>> = HashSet::new();
    let mut stage0: HashMap<Vec<u8>, IndexEntry> = HashMap::new();
    for e in &index.entries {
        path_keys.insert(e.path.clone());
        if e.stage() != 0 {
            unmerged_paths.insert(e.path.clone());
        } else {
            stage0.insert(e.path.clone(), e.clone());
        }
    }

    let mut symlink_parent_cache: HashMap<PathBuf, bool> = HashMap::new();
    let mut summary = StageTrackedSummary::default();

    for raw_path in path_keys {
        let abs_path = worktree_path_from_index_rel(work_tree, &raw_path);
        if path_has_symlink_parent_cached(work_tree, &abs_path, &mut symlink_parent_cache) {
            if index.remove(&raw_path) {
                summary.removed.push(raw_path.clone());
            }
            continue;
        }

        if unmerged_paths.contains(&raw_path) {
            refresh_unmerged_tracked_path(repo, index, &raw_path, &abs_path, &mut summary)?;
            continue;
        }

        let Some(idx_e) = stage0.get(&raw_path) else {
            continue;
        };

        if fs::symlink_metadata(&abs_path).is_ok() {
            let ctx = PresentRefreshCtx {
                repo,
                index,
                raw_path: &raw_path,
                abs_path: &abs_path,
                idx_e,
                index_mtime,
            };
            if refresh_present_tracked_path(ctx)? {
                summary.modified.push(raw_path);
            }
        } else if idx_e.skip_worktree() {
            continue;
        } else if index.remove(&raw_path) {
            summary.removed.push(raw_path);
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
    index.remove(raw_path);
    if fs::symlink_metadata(abs_path).is_ok() {
        if idx_mode == 0o160000 {
            if let Some(oid) = read_submodule_head_oid(abs_path) {
                stage_gitlink_from_stat(abs_path, raw_path, oid, index)?;
                summary.modified.push(raw_path.to_vec());
            }
        } else {
            stage_blob_from_worktree(repo, index, abs_path, raw_path, None)?;
            summary.modified.push(raw_path.to_vec());
        }
    } else {
        summary.removed.push(raw_path.to_vec());
    }
    Ok(())
}

fn refresh_present_tracked_path(ctx: PresentRefreshCtx<'_>) -> Result<bool> {
    let PresentRefreshCtx {
        repo,
        index,
        raw_path,
        abs_path,
        idx_e,
        index_mtime,
    } = ctx;
    let idx_mode = idx_e.mode;
    let idx_intent_to_add = idx_e.intent_to_add();
    let idx_oid = idx_e.oid;

    if idx_mode == 0o160000 {
        return refresh_gitlink(repo, index, raw_path, abs_path, idx_e);
    }

    let meta = fs::symlink_metadata(abs_path)?;
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

    let wt_mode = mode_from_metadata(&meta);
    let stat_same = stat_matches(idx_e, &meta);
    let racy = entry_is_racy(idx_e, index_mtime);
    if !idx_intent_to_add && idx_e.size != 0 && stat_same && !racy {
        if wt_mode == idx_mode {
            return Ok(false);
        }
        let entry = entry_from_stat(abs_path, raw_path, idx_oid, wt_mode)?;
        index.stage_file(entry);
        return Ok(true);
    }

    let oid = read_worktree_blob_oid(repo, abs_path, &meta)?;
    if !idx_intent_to_add && idx_oid == oid && wt_mode == idx_mode {
        return Ok(false);
    }
    let entry = entry_from_stat(abs_path, raw_path, oid, wt_mode)?;
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
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(abs_path)?;
    let entry = IndexEntry {
        ctime_sec: meta.ctime() as u32,
        ctime_nsec: meta.ctime_nsec() as u32,
        mtime_sec: meta.mtime() as u32,
        mtime_nsec: meta.mtime_nsec() as u32,
        dev: meta.dev() as u32,
        ino: meta.ino() as u32,
        mode: 0o160000,
        uid: meta.uid(),
        gid: meta.gid(),
        size: 0,
        oid,
        flags: raw_path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: raw_path.to_vec(),
        base_index_pos: 0,
    };
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
        fs::read(abs_path)?
    };
    repo.odb.write(ObjectKind::Blob, &data)
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

    use crate::diff::{entry_is_racy, smudge_racily_clean_entries, stat_matches};
    use crate::index::{entry_from_metadata, index_file_mtime, MODE_EXECUTABLE, MODE_REGULAR};
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
        let stale_oid = Odb::hash_object_data(ObjectKind::Blob, content_a);
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
        let stale_oid = Odb::hash_object_data(ObjectKind::Blob, content_a);

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
