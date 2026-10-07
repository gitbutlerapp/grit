//! Directory-oriented worktree stat reads for index comparison.
//!
//! Groups indexed blob paths by parent directory and uses one `read_dir` per directory,
//! reusing `DirEntry` metadata for tracked files instead of a separate `symlink_metadata`
//! per index path.
//!
//! When [`WorktreeBlobScanOptions::preload_index`] is true and the tracked blob count is at
//! least [`PARALLEL_STAT_MIN_ENTRIES`], stat collection runs in contiguous index-order chunks
//! across a small number of threads (`std::thread::scope`). Visit order stays directory-sorted
//! so diff output matches the serial path; only the metadata probes are parallelized. A later
//! shared thread pool can replace the scoped helper without changing callers.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Test hook: incremented once per worktree metadata syscall used by directory scans.
#[cfg(test)]
pub(crate) static WORKTREE_METADATA_PROBE: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_worktree_metadata_probe() {
    WORKTREE_METADATA_PROBE.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn worktree_metadata_probe_count() -> usize {
    WORKTREE_METADATA_PROBE.load(Ordering::Relaxed)
}

/// Minimum tracked blob entries before parallel stat preload is considered.
pub const PARALLEL_STAT_MIN_ENTRIES: usize = 1000;

/// Upper bound on worker threads for parallel stat preload (see Git's small preload pool).
const STAT_PARALLEL_THREAD_CAP: usize = 4;

/// Parallelism controls for directory-grouped blob scans (honors `core.preloadIndex`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WorktreeBlobScanOptions {
    /// When false, stat preload never uses more than one thread.
    pub preload_index: bool,
    /// When `Some(1)`, forces a serial scan. When `Some(n > 1)`, caps workers at
    /// [`STAT_PARALLEL_THREAD_CAP`]. When `None`, picks from [`std::thread::available_parallelism`].
    pub stat_parallel_threads: Option<usize>,
}

/// Test hook: worker threads used by the most recent blob scan (`1` = serial).
#[doc(hidden)]
pub static LAST_BLOB_SCAN_THREADS: AtomicUsize = AtomicUsize::new(1);

/// Returns [`LAST_BLOB_SCAN_THREADS`] for tests (embedders should not rely on this).
#[doc(hidden)]
#[must_use]
pub fn last_blob_scan_threads_for_tests() -> usize {
    LAST_BLOB_SCAN_THREADS.load(Ordering::Relaxed)
}

/// Returns how many threads a blob scan should use for `blob_count` entries.
#[must_use]
pub(crate) fn stat_scan_thread_count(options: WorktreeBlobScanOptions, blob_count: usize) -> usize {
    let chosen = if !options.preload_index || blob_count < PARALLEL_STAT_MIN_ENTRIES {
        1
    } else if let Some(requested) = options.stat_parallel_threads {
        if requested <= 1 {
            1
        } else {
            requested.min(STAT_PARALLEL_THREAD_CAP)
        }
    } else {
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        cpus.clamp(1, STAT_PARALLEL_THREAD_CAP)
    };
    LAST_BLOB_SCAN_THREADS.store(chosen, Ordering::Relaxed);
    chosen
}

fn probe_worktree_metadata_call() {
    #[cfg(test)]
    WORKTREE_METADATA_PROBE.fetch_add(1, Ordering::Relaxed);
}

fn dir_entry_metadata(entry: &fs::DirEntry) -> io::Result<fs::Metadata> {
    probe_worktree_metadata_call();
    entry.metadata()
}

fn symlink_metadata_path(path: &Path) -> io::Result<fs::Metadata> {
    probe_worktree_metadata_call();
    fs::symlink_metadata(path)
}

use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};

/// A tracked blob path grouped under its parent directory (repository-relative).
#[derive(Debug, Clone)]
pub(crate) struct BlobInDir {
    /// File name within the directory (no slashes).
    pub file_name: String,
    /// Full repository-relative path.
    pub rel_path: String,
    /// Index into [`Index::entries`].
    pub entry_index: usize,
}

/// Group stage-0 blob index entries by parent directory for bulk `read_dir`.
#[must_use]
pub(crate) fn group_blob_entries_by_dir(index: &Index) -> HashMap<String, Vec<BlobInDir>> {
    let mut by_dir: HashMap<String, Vec<BlobInDir>> = HashMap::new();
    for (entry_index, ie) in index.entries.iter().enumerate() {
        if ie.stage() != 0 {
            continue;
        }
        if ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
            continue;
        }
        if ie.mode == MODE_GITLINK {
            continue;
        }
        let Ok(rel_path) = std::str::from_utf8(&ie.path) else {
            continue;
        };
        let (dir, file_name) = split_dir_file(rel_path);
        by_dir.entry(dir).or_default().push(BlobInDir {
            file_name,
            rel_path: rel_path.to_owned(),
            entry_index,
        });
    }
    by_dir
}

fn split_dir_file(rel_path: &str) -> (String, String) {
    match rel_path.rsplit_once('/') {
        Some((dir, file)) => (dir.to_owned(), file.to_owned()),
        None => (String::new(), rel_path.to_owned()),
    }
}

/// Result of looking up one indexed blob on disk during a directory scan.
pub(crate) enum BlobDiskLookup {
    /// Metadata for the indexed path.
    Present(fs::Metadata),
    /// Missing path or non-directory parent (`NotFound` / `ENOTDIR`).
    Missing,
    /// Other I/O error.
    Io(io::Error),
}

/// Collect every scannable blob in ascending index order.
#[must_use]
fn index_ordered_blobs(by_dir: &HashMap<String, Vec<BlobInDir>>) -> Vec<BlobInDir> {
    let mut blobs: Vec<BlobInDir> = by_dir.values().flatten().cloned().collect();
    blobs.sort_by_key(|b| b.entry_index);
    blobs
}

fn group_blobs_chunk(chunk: &[BlobInDir]) -> HashMap<String, Vec<BlobInDir>> {
    let mut partial: HashMap<String, Vec<BlobInDir>> = HashMap::new();
    for blob in chunk {
        let (dir, _) = split_dir_file(&blob.rel_path);
        partial.entry(dir).or_default().push(blob.clone());
    }
    partial
}

fn preload_blob_lookups_parallel(
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    threads: usize,
    symlink_missing: &HashSet<String>,
    dir_abs: Arc<dyn Fn(&str) -> PathBuf + Send + Sync>,
    file_abs: Arc<dyn Fn(&str) -> PathBuf + Send + Sync>,
) -> Result<HashMap<usize, BlobDiskLookup>> {
    let ordered = index_ordered_blobs(by_dir);
    if ordered.is_empty() {
        return Ok(HashMap::new());
    }
    let chunk_size = ordered.len().div_ceil(threads);
    let mut merged = HashMap::new();
    let mut worker_panic = false;
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in ordered.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let symlink_missing = symlink_missing.clone();
            let dir_abs = Arc::clone(&dir_abs);
            let file_abs = Arc::clone(&file_abs);
            handles.push(scope.spawn(move || {
                let partial_dirs = group_blobs_chunk(&chunk);
                let mut local = HashMap::new();
                let _ = for_each_blob_by_directory(
                    &partial_dirs,
                    |dir| dir_abs(dir),
                    |rel| file_abs(rel),
                    |rel| symlink_missing.contains(rel),
                    |entry_index, _path, lookup| {
                        local.insert(entry_index, lookup);
                        Ok(())
                    },
                );
                local
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(local) => merged.extend(local),
                Err(_) => worker_panic = true,
            }
        }
    });
    if worker_panic {
        return Err(Error::Message(
            "parallel blob stat worker thread panicked".to_owned(),
        ));
    }
    Ok(merged)
}

/// Visit grouped blob entries with one `read_dir` per distinct parent directory.
///
/// When `has_symlink_ancestor(rel_path)` is true, the entry is reported as [`BlobDiskLookup::Missing`]
/// without extra I/O (symlink replaced a directory on the path).
pub(crate) fn for_each_blob_by_directory<F>(
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    dir_abs: impl Fn(&str) -> PathBuf,
    file_abs: impl Fn(&str) -> PathBuf,
    mut has_symlink_ancestor: impl FnMut(&str) -> bool,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, &str, BlobDiskLookup) -> Result<()>,
{
    let mut dirs: Vec<_> = by_dir.keys().cloned().collect();
    dirs.sort();
    for dir in dirs {
        let Some(blobs) = by_dir.get(&dir) else {
            continue;
        };
        // When a tracked directory is replaced by a symlink, Git reports indexed paths as
        // deleted without traversing the link target (even if the target is unreadable).
        if blobs.iter().all(|b| has_symlink_ancestor(&b.rel_path)) {
            for blob in blobs {
                visit(blob.entry_index, &blob.rel_path, BlobDiskLookup::Missing)?;
            }
            continue;
        }

        let abs_dir = dir_abs(&dir);
        let (dir_readable, on_disk) = read_directory_map(&abs_dir)?;
        for blob in blobs {
            if has_symlink_ancestor(&blob.rel_path) {
                visit(blob.entry_index, &blob.rel_path, BlobDiskLookup::Missing)?;
                continue;
            }
            let target = file_abs(&blob.rel_path);
            let lookup = if !dir_readable {
                BlobDiskLookup::Missing
            } else {
                lookup_blob_in_directory(&on_disk, &blob.file_name, &target)
            };
            visit(blob.entry_index, &blob.rel_path, lookup)?;
        }
    }
    Ok(())
}

/// Like [`for_each_blob_by_directory`], optionally preloading disk lookups in parallel.
pub(crate) fn for_each_blob_by_directory_parallel<F>(
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    scan_options: WorktreeBlobScanOptions,
    dir_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    mut has_symlink_ancestor: impl FnMut(&str) -> bool,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, &str, BlobDiskLookup) -> Result<()>,
{
    let ordered = index_ordered_blobs(by_dir);
    let mut symlink_missing = HashSet::new();
    for blob in &ordered {
        if has_symlink_ancestor(&blob.rel_path) {
            symlink_missing.insert(blob.rel_path.clone());
        }
    }

    let blob_count = ordered.len();
    let threads = stat_scan_thread_count(scan_options, blob_count);
    let dir_abs_arc: Arc<dyn Fn(&str) -> PathBuf + Send + Sync> = Arc::new(dir_abs);
    let file_abs_arc: Arc<dyn Fn(&str) -> PathBuf + Send + Sync> = Arc::new(file_abs);
    let mut preloaded = if threads > 1 {
        preload_blob_lookups_parallel(
            by_dir,
            threads,
            &symlink_missing,
            Arc::clone(&dir_abs_arc),
            Arc::clone(&file_abs_arc),
        )?
    } else {
        HashMap::new()
    };
    let use_preloaded = threads > 1;

    for_each_blob_by_directory(
        by_dir,
        |dir| dir_abs_arc(dir),
        |rel| file_abs_arc(rel),
        |rel| has_symlink_ancestor(rel),
        |entry_index, path, lookup| {
            let lookup = if use_preloaded {
                preloaded.remove(&entry_index).unwrap_or(lookup)
            } else {
                lookup
            };
            visit(entry_index, path, lookup)
        },
    )
}

fn lookup_blob_in_directory(
    on_disk: &HashMap<String, fs::DirEntry>,
    index_file_name: &str,
    target_abs: &Path,
) -> BlobDiskLookup {
    let try_entry = |entry: &fs::DirEntry| -> Option<std::result::Result<fs::Metadata, io::Error>> {
        if entry.path() == target_abs {
            Some(dir_entry_metadata(entry))
        } else {
            None
        }
    };

    if let Some(entry) = on_disk.get(index_file_name) {
        if let Some(meta) = try_entry(entry) {
            return match meta {
                Ok(m) => BlobDiskLookup::Present(m),
                Err(e) => BlobDiskLookup::Io(e),
            };
        }
    }
    for entry in on_disk.values() {
        if let Some(meta) = try_entry(entry) {
            return match meta {
                Ok(m) => BlobDiskLookup::Present(m),
                Err(e) => BlobDiskLookup::Io(e),
            };
        }
    }
    match symlink_metadata_path(target_abs) {
        Ok(meta) => BlobDiskLookup::Present(meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
            BlobDiskLookup::Missing
        }
        Err(e) => BlobDiskLookup::Io(e),
    }
}

fn read_directory_map(abs_dir: &Path) -> Result<(bool, HashMap<String, fs::DirEntry>)> {
    match fs::read_dir(abs_dir) {
        Ok(rd) => {
            let mut on_disk = HashMap::new();
            for item in rd {
                let Ok(entry) = item else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                on_disk.insert(name, entry);
            }
            Ok((true, on_disk))
        }
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::PermissionDenied
                || e.raw_os_error() == Some(20) =>
        {
            Ok((false, HashMap::new()))
        }
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{entry_from_stat, Index};
    use crate::objects::{ObjectId, ObjectKind};
    use crate::odb::Odb;
    use std::fs;
    use tempfile::TempDir;

    fn write_blob_file(dir: &Path, rel: &str, bytes: &[u8]) -> ObjectId {
        let odb = Odb::new(&dir.join(".git/objects"));
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, bytes).unwrap();
        odb.write(ObjectKind::Blob, bytes).unwrap()
    }

    #[test]
    fn dir_grouped_scan_matches_clean_tree() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "d0/f0.txt", b"hello\n");
        let mut index = Index::new();
        index.add_or_replace(
            entry_from_stat(&wt.join("d0/f0.txt"), b"d0/f0.txt", oid, 0o100644).unwrap(),
        );
        let grouped = group_blob_entries_by_dir(&index);
        assert_eq!(grouped.len(), 1);
        assert!(grouped.contains_key("d0"));

        let mut seen = 0;
        let dir_abs = |dir: &str| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                wt.join(dir)
            }
        };
        let file_abs = |rel: &str| wt.join(rel);
        for_each_blob_by_directory(
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            |entry_index, path, lookup| {
                assert_eq!(path, "d0/f0.txt");
                assert_eq!(index.entries[entry_index].oid, oid);
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn dir_grouped_scan_reports_missing_file() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = ObjectId::zero();
        let mut index = Index::new();
        fs::write(wt.join("gone.txt"), b"x").unwrap();
        let entry = entry_from_stat(&wt.join("gone.txt"), b"gone.txt", oid, 0o100644).unwrap();
        index.add_or_replace(entry);
        fs::remove_file(wt.join("gone.txt")).unwrap();

        let grouped = group_blob_entries_by_dir(&index);
        let mut missing = false;
        let dir_abs = |dir: &str| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                wt.join(dir)
            }
        };
        let file_abs = |rel: &str| wt.join(rel);
        for_each_blob_by_directory(
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            |_, _, lookup| {
                missing = matches!(lookup, BlobDiskLookup::Missing);
                Ok(())
            },
        )
        .unwrap();
        assert!(missing);
    }

    #[test]
    fn core_preload_index_false_uses_single_thread() {
        let threads = stat_scan_thread_count(
            WorktreeBlobScanOptions {
                preload_index: false,
                stat_parallel_threads: Some(4),
            },
            PARALLEL_STAT_MIN_ENTRIES,
        );
        assert_eq!(threads, 1);
    }

    #[cfg(unix)]
    #[test]
    fn directory_scan_symlink_to_unreadable_target_reports_missing() {
        use crate::diff::{
            diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus,
        };
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "dir/file.txt", b"tracked\n");
        let mut index = Index::new();
        index.add_or_replace(
            entry_from_stat(&wt.join("dir/file.txt"), b"dir/file.txt", oid, 0o100644).unwrap(),
        );

        fs::remove_dir_all(wt.join("dir")).unwrap();
        let secret = wt.join("secret");
        fs::create_dir(&secret).unwrap();
        fs::write(secret.join("file.txt"), b"other\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0)).unwrap();
        std::os::unix::fs::symlink(&secret, wt.join("dir")).unwrap();

        let odb = Odb::new(&wt.join(".git/objects"));
        let (diffs, _) = diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            DiffIndexToWorktreeOptions {
                index_mtime: None,
                ignore_submodule_untracked: false,
                simplify_gitlinks: false,
                error_on_broken_gitlinks: false,
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("status diff must not traverse unreadable symlink target");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].status, DiffStatus::Deleted);
        assert_eq!(diffs[0].old_path.as_deref(), Some("dir/file.txt"));
    }
}
