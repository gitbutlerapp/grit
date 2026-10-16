//! Directory-oriented worktree stat reads for index comparison.
//!
//! Groups indexed blob paths by parent directory and uses one `read_dir` per directory,
//! reusing `DirEntry` metadata for tracked files instead of a separate `symlink_metadata`
//! per index path.
//!
//! When [`WorktreeBlobScanOptions::preload_index`] is true and the tracked blob count is at
//! least [`PARALLEL_STAT_MIN_ENTRIES`], stat collection preloads disk lookups in parallel by
//! partitioning sorted parent-directory keys across a small thread pool (`std::thread::scope`).
//! Visit order stays directory-sorted so diff output matches the serial path; only the metadata
//! probes are parallelized. A later shared thread pool can replace the scoped helper without
//! changing callers.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Minimum blob entries sharing a parent before `read_dir` amortizes separate stats.
/// Deep trees often have one tracked file per leaf directory; `read_dir` still beats a
/// separate stat per file when grouping parallel preload by directory.
#[cfg(windows)]
const READ_DIR_AMORTIZE_MIN: usize = 2;
#[cfg(not(windows))]
const READ_DIR_AMORTIZE_MIN: usize = 2;

/// Test hook: incremented once per worktree metadata syscall used by directory scans.
#[cfg(test)]
pub(crate) static WORKTREE_METADATA_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Test hook: `read_dir` calls during directory-grouped blob scans.
#[cfg(test)]
pub(crate) static WORKTREE_READ_DIR_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Only syscalls under this root are counted, so tests scanning other worktrees in
/// parallel do not inflate a probing test's counts.
#[cfg(test)]
static WORKTREE_PROBE_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Serializes probing tests; each owns the counters while its scope is alive.
#[cfg(test)]
static WORKTREE_PROBE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Keeps worktree syscall probing armed for one test; disarms on drop.
#[cfg(test)]
pub(crate) struct WorktreeProbeScope {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for WorktreeProbeScope {
    fn drop(&mut self) {
        *WORKTREE_PROBE_ROOT
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Zero the probe counters and count only syscalls on paths under `root`.
#[cfg(test)]
pub(crate) fn reset_worktree_metadata_probe(root: &Path) -> WorktreeProbeScope {
    let serial = WORKTREE_PROBE_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *WORKTREE_PROBE_ROOT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(root.to_path_buf());
    WORKTREE_METADATA_PROBE.store(0, Ordering::Relaxed);
    WORKTREE_READ_DIR_PROBE.store(0, Ordering::Relaxed);
    WorktreeProbeScope { _serial: serial }
}

#[cfg(test)]
fn probe_counts(path: &Path) -> bool {
    WORKTREE_PROBE_ROOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .is_some_and(|root| path.starts_with(root))
}

#[cfg(test)]
pub(crate) fn worktree_metadata_probe_count() -> usize {
    WORKTREE_METADATA_PROBE.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn worktree_read_dir_probe_count() -> usize {
    WORKTREE_READ_DIR_PROBE.load(Ordering::Relaxed)
}

/// Minimum tracked blob entries before parallel stat preload is considered.
pub const PARALLEL_STAT_MIN_ENTRIES: usize = 1000;

/// Parallelism controls for directory-grouped blob scans (honors `core.preloadIndex`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WorktreeBlobScanOptions {
    /// When false, stat preload never uses more than one thread.
    pub preload_index: bool,
    /// When `Some(1)`, forces a serial scan. When `Some(n > 1)`, caps workers at `n`.
    /// When `None`, uses [`crate::parallel::ThreadPolicy::index_stat_preload_threads`].
    pub stat_parallel_threads: Option<usize>,
}

/// Returns how many threads a blob scan should use for `blob_count` entries.
#[must_use]
pub(crate) fn stat_scan_thread_count(
    policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    options: WorktreeBlobScanOptions,
    blob_count: usize,
) -> usize {
    if !options.preload_index || blob_count < PARALLEL_STAT_MIN_ENTRIES {
        return 1;
    }
    policy.index_stat_preload_threads(config, blob_count, options.stat_parallel_threads)
}

fn probe_worktree_metadata_call(_path: &Path) {
    #[cfg(test)]
    if probe_counts(_path) {
        WORKTREE_METADATA_PROBE.fetch_add(1, Ordering::Relaxed);
    }
}

fn dir_entry_metadata(entry: &fs::DirEntry) -> io::Result<fs::Metadata> {
    probe_worktree_metadata_call(&entry.path());
    entry.metadata()
}

fn symlink_metadata_path(path: &Path) -> io::Result<fs::Metadata> {
    probe_worktree_metadata_call(path);
    fs::symlink_metadata(path)
}

use std::collections::BTreeSet;

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
pub(crate) fn group_blob_entries_by_dir(
    index: &Index,
    honor_fsmonitor_valid: bool,
) -> HashMap<String, Vec<BlobInDir>> {
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
        if honor_fsmonitor_valid && index.fsmonitor_last_update.is_some() && ie.fsmonitor_valid() {
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

/// Group every stage-0 blob index entry by parent directory, including assume-unchanged and
/// skip-worktree rows excluded from [`group_blob_entries_by_dir`].
#[must_use]
pub(crate) fn group_all_stage0_blob_entries_by_dir(
    index: &Index,
) -> HashMap<String, Vec<BlobInDir>> {
    let mut by_dir: HashMap<String, Vec<BlobInDir>> = HashMap::new();
    for (entry_index, ie) in index.entries.iter().enumerate() {
        if ie.stage() != 0 || ie.intent_to_add() || ie.mode == MODE_GITLINK {
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

/// Discover untracked siblings in directories whose indexed blobs are only assume-unchanged or
/// skip-worktree (and therefore omitted from the fused tracked stat scan).
fn index_has_stat_skipped_stage0_blob(index: &Index) -> bool {
    index.entries.iter().any(|e| {
        e.stage() == 0
            && !e.intent_to_add()
            && e.mode != MODE_GITLINK
            && (e.skip_worktree() || e.assume_unchanged())
    })
}

pub(crate) fn supplement_untracked_beside_stat_skipped_indexed_dirs(
    index: &Index,
    work_tree: &Path,
    out: &mut Vec<String>,
) -> Result<()> {
    if !index_has_stat_skipped_stage0_blob(index) {
        return Ok(());
    }
    let scanned_dirs = group_blob_entries_by_dir(index, false);
    let all_dirs = group_all_stage0_blob_entries_by_dir(index);
    let mut dirs: Vec<String> = all_dirs
        .keys()
        .filter(|dir| !scanned_dirs.contains_key(*dir))
        .cloned()
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some(blobs) = all_dirs.get(&dir) else {
            continue;
        };
        let abs_dir = if dir.is_empty() {
            work_tree.to_path_buf()
        } else {
            work_tree.join(&dir)
        };
        let (dir_readable, on_disk) = read_directory_map(&abs_dir)?;
        if dir_readable {
            let paths = stage0_indexed_paths(index);
            out.extend(collect_untracked_siblings_in_dir(
                &dir, blobs, &on_disk, &paths,
            ));
        }
    }
    Ok(())
}

/// Like [`group_blob_entries_by_dir`], but only for repository-relative paths in `paths`.
#[must_use]
pub(crate) fn group_blob_entries_by_dir_for_paths(
    index: &Index,
    paths: &BTreeSet<String>,
) -> HashMap<String, Vec<BlobInDir>> {
    let mut by_dir: HashMap<String, Vec<BlobInDir>> = HashMap::new();
    for path in paths {
        let path_bytes = path.as_bytes();
        let Some(ie) = index.get(path_bytes, 0) else {
            continue;
        };
        if ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
            continue;
        }
        if ie.mode == MODE_GITLINK {
            continue;
        }
        let Ok(entry_index) = index.entries.binary_search_by(|e| {
            e.path
                .as_slice()
                .cmp(path_bytes)
                .then_with(|| e.stage().cmp(&0))
        }) else {
            continue;
        };
        let (dir, file_name) = split_dir_file(path);
        by_dir.entry(dir).or_default().push(BlobInDir {
            file_name,
            rel_path: path.clone(),
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

/// Collect scannable blob `(entry_index, rel_path)` pairs in ascending index order.
#[must_use]
fn index_ordered_blob_paths(by_dir: &HashMap<String, Vec<BlobInDir>>) -> Vec<(usize, String)> {
    let mut blobs: Vec<(usize, String)> = by_dir
        .values()
        .flat_map(|v| v.iter().map(|b| (b.entry_index, b.rel_path.clone())))
        .collect();
    blobs.sort_by_key(|(entry_index, _)| *entry_index);
    blobs
}

#[allow(clippy::type_complexity)]
fn preload_blob_lookups_parallel(
    indexed_stage0_paths: Option<Arc<HashSet<Vec<u8>>>>,
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    entry_slots: usize,
    threads: usize,
    symlink_missing: &HashSet<String>,
    dir_abs: Arc<dyn Fn(&str) -> PathBuf + Send + Sync>,
    file_abs: Arc<dyn Fn(&str) -> PathBuf + Send + Sync>,
) -> Result<(Vec<Option<BlobDiskLookup>>, Vec<String>)> {
    if by_dir.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut dirs: Vec<String> = by_dir.keys().cloned().collect();
    dirs.sort();
    let mut lookups: Vec<Option<BlobDiskLookup>> = (0..entry_slots).map(|_| None).collect();
    let chunk_size = dirs.len().div_ceil(threads);
    let merged_untracked = std::thread::scope(|scope| -> Result<Vec<String>> {
        let mut handles = Vec::new();
        for dir_chunk in dirs.chunks(chunk_size) {
            let mut partial_dirs = HashMap::new();
            for dir in dir_chunk {
                if let Some(blobs) = by_dir.get(dir) {
                    partial_dirs.insert(dir.clone(), blobs.clone());
                }
            }
            let symlink_missing = symlink_missing.clone();
            let dir_abs = Arc::clone(&dir_abs);
            let file_abs = Arc::clone(&file_abs);
            let paths = indexed_stage0_paths.as_ref().map(Arc::clone);
            handles.push(scope.spawn(
                move || -> Result<(Vec<(usize, BlobDiskLookup)>, Vec<String>)> {
                    let mut local =
                        Vec::with_capacity(partial_dirs.values().map(|v| v.len()).sum::<usize>());
                    let mut local_untracked = Vec::new();
                    for_each_blob_by_directory(
                        paths.as_deref(),
                        None,
                        None,
                        &partial_dirs,
                        |dir| dir_abs(dir),
                        |rel| file_abs(rel),
                        |rel| symlink_missing.contains(rel),
                        Some(&mut local_untracked),
                        |entry_index, _path, lookup| {
                            local.push((entry_index, lookup));
                            Ok(())
                        },
                    )?;
                    Ok((local, local_untracked))
                },
            ));
        }
        let mut merged_untracked = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok((local, local_untracked))) => {
                    merged_untracked.extend(local_untracked);
                    for (entry_index, lookup) in local {
                        if entry_index >= lookups.len() {
                            return Err(Error::Message(format!(
                                "parallel blob stat preload out-of-range index entry {entry_index}"
                            )));
                        }
                        lookups[entry_index] = Some(lookup);
                    }
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Error::Message(
                        "parallel blob stat worker thread panicked".to_owned(),
                    ));
                }
            }
        }
        Ok(merged_untracked)
    })?;
    Ok((lookups, merged_untracked))
}

/// Visit grouped blob entries with one `read_dir` per distinct parent directory.
///
/// When `has_symlink_ancestor(rel_path)` is true, the entry is reported as [`BlobDiskLookup::Missing`]
/// without extra I/O (symlink replaced a directory on the path).
fn collect_untracked_siblings_in_dir(
    dir: &str,
    blobs: &[BlobInDir],
    on_disk: &HashMap<String, fs::DirEntry>,
    indexed_stage0_paths: &HashSet<Vec<u8>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let tracked: HashSet<&str> = blobs.iter().map(|b| b.file_name.as_str()).collect();
    for (name, entry) in on_disk {
        if name == ".git" {
            continue;
        }
        let rel = if dir.is_empty() {
            name.clone()
        } else {
            format!("{dir}/{name}")
        };
        if indexed_stage0_paths.contains(rel.as_bytes()) {
            continue;
        }
        if tracked.contains(name.as_str()) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            out.push(rel);
            continue;
        }
        if file_type.is_dir() {
            out.push(format!("{rel}/"));
        } else if file_type.is_file() {
            out.push(rel);
        }
    }
    out
}

/// Untracked paths inside a worktree directory (used when a tracked file path became a directory).
pub(crate) fn collect_untracked_children_in_directory(
    work_tree: &Path,
    dir_rel: &str,
    indexed_stage0_paths: &HashSet<Vec<u8>>,
) -> Result<Vec<String>> {
    let abs = if dir_rel.is_empty() {
        work_tree.to_path_buf()
    } else {
        work_tree.join(dir_rel)
    };
    let (readable, on_disk) = read_directory_map(&abs)?;
    if !readable {
        return Ok(Vec::new());
    }
    Ok(collect_untracked_siblings_in_dir(
        dir_rel,
        &[],
        &on_disk,
        indexed_stage0_paths,
    ))
}

#[must_use]
pub(crate) fn stage0_indexed_paths(index: &Index) -> HashSet<Vec<u8>> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect()
}

#[must_use]
pub(crate) fn stage0_indexed_blob_modes(index: &Index) -> HashMap<String, u32> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .filter_map(|e| {
            std::str::from_utf8(&e.path)
                .ok()
                .map(|p| (p.to_owned(), e.mode))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn for_each_blob_by_directory<F>(
    indexed_stage0_paths: Option<&HashSet<Vec<u8>>>,
    indexed_blob_modes: Option<&HashMap<String, u32>>,
    work_tree_for_untracked: Option<&Path>,
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    dir_abs: impl Fn(&str) -> PathBuf,
    file_abs: impl Fn(&str) -> PathBuf,
    mut has_symlink_ancestor: impl FnMut(&str) -> bool,
    mut side_untracked: Option<&mut Vec<String>>,
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

        if blobs.len() < READ_DIR_AMORTIZE_MIN {
            if let (Some(out), Some(paths)) = (side_untracked.as_deref_mut(), indexed_stage0_paths)
            {
                let abs_dir = dir_abs(&dir);
                let (dir_readable, on_disk) = read_directory_map(&abs_dir)?;
                if dir_readable {
                    out.extend(collect_untracked_siblings_in_dir(
                        &dir, blobs, &on_disk, paths,
                    ));
                }
            }
            for blob in blobs {
                if has_symlink_ancestor(&blob.rel_path) {
                    visit(blob.entry_index, &blob.rel_path, BlobDiskLookup::Missing)?;
                    continue;
                }
                let target = file_abs(&blob.rel_path);
                let lookup = match symlink_metadata_path(&target) {
                    Ok(meta) => BlobDiskLookup::Present(meta),
                    Err(e)
                        if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) =>
                    {
                        BlobDiskLookup::Missing
                    }
                    Err(e) => BlobDiskLookup::Io(e),
                };
                maybe_collect_file_to_directory_untracked(
                    indexed_stage0_paths,
                    indexed_blob_modes,
                    work_tree_for_untracked,
                    side_untracked.as_deref_mut(),
                    &blob.rel_path,
                    &lookup,
                )?;
                visit(blob.entry_index, &blob.rel_path, lookup)?;
            }
            continue;
        }

        let abs_dir = dir_abs(&dir);
        let (dir_readable, on_disk) = read_directory_map(&abs_dir)?;
        if let (Some(out), Some(paths)) = (side_untracked.as_deref_mut(), indexed_stage0_paths) {
            if dir_readable {
                out.extend(collect_untracked_siblings_in_dir(
                    &dir, blobs, &on_disk, paths,
                ));
            }
        }
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
            maybe_collect_file_to_directory_untracked(
                indexed_stage0_paths,
                indexed_blob_modes,
                work_tree_for_untracked,
                side_untracked.as_deref_mut(),
                &blob.rel_path,
                &lookup,
            )?;
            visit(blob.entry_index, &blob.rel_path, lookup)?;
        }
    }
    Ok(())
}

fn maybe_collect_file_to_directory_untracked(
    indexed_stage0_paths: Option<&HashSet<Vec<u8>>>,
    indexed_blob_modes: Option<&HashMap<String, u32>>,
    work_tree: Option<&Path>,
    side_untracked: Option<&mut Vec<String>>,
    rel_path: &str,
    lookup: &BlobDiskLookup,
) -> Result<()> {
    let Some(out) = side_untracked else {
        return Ok(());
    };
    let (Some(paths), Some(modes), Some(wt)) =
        (indexed_stage0_paths, indexed_blob_modes, work_tree)
    else {
        return Ok(());
    };
    let BlobDiskLookup::Present(meta) = lookup else {
        return Ok(());
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(());
    }
    let Some(mode) = modes.get(rel_path) else {
        return Ok(());
    };
    if (*mode & 0o170000) != 0o100000 {
        return Ok(());
    }
    let abs = wt.join(rel_path);
    if abs.join(".git").exists() {
        return Ok(());
    }
    out.extend(collect_untracked_children_in_directory(
        wt, rel_path, paths,
    )?);
    Ok(())
}

/// Like [`for_each_blob_by_directory`], optionally preloading disk lookups in parallel.
#[allow(clippy::too_many_arguments)]
pub(crate) fn for_each_blob_by_directory_parallel<F>(
    indexed_stage0_paths: Option<&HashSet<Vec<u8>>>,
    indexed_blob_modes: Option<&HashMap<String, u32>>,
    work_tree_for_untracked: Option<&Path>,
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    scan_options: WorktreeBlobScanOptions,
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    scan_symlink_ancestors: bool,
    dir_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    mut has_symlink_ancestor: impl FnMut(&str) -> bool,
    mut side_untracked: Option<&mut Vec<String>>,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, &str, BlobDiskLookup) -> Result<()>,
{
    let ordered = index_ordered_blob_paths(by_dir);
    let mut symlink_missing = HashSet::new();
    if scan_symlink_ancestors {
        // One ancestor probe per distinct parent directory (not per indexed file).
        for blobs in by_dir.values() {
            let Some(sample) = blobs.first() else {
                continue;
            };
            if has_symlink_ancestor(&sample.rel_path) {
                for blob in blobs {
                    symlink_missing.insert(blob.rel_path.clone());
                }
            }
        }
    }

    let blob_count = ordered.len();
    let threads = stat_scan_thread_count(thread_policy, config, scan_options, blob_count);
    let dir_abs_arc: Arc<dyn Fn(&str) -> PathBuf + Send + Sync> = Arc::new(dir_abs);
    let file_abs_arc: Arc<dyn Fn(&str) -> PathBuf + Send + Sync> = Arc::new(file_abs);
    if threads > 1 {
        // Always partition parallel preload by directory groups so multi-file parents use
        // one read_dir per directory (Git preload-index style). A flat per-file stat loop
        // regresses deep leaf-dir layouts (~50k paths, few files per directory).
        let entry_slots = ordered
            .iter()
            .map(|(entry_index, _)| *entry_index)
            .max()
            .map_or(0, |i| i + 1);
        let paths_arc = indexed_stage0_paths.map(|set| Arc::new(set.clone()));
        let (mut preloaded, parallel_untracked) = preload_blob_lookups_parallel(
            paths_arc,
            by_dir,
            entry_slots,
            threads,
            &symlink_missing,
            Arc::clone(&dir_abs_arc),
            Arc::clone(&file_abs_arc),
        )?;
        if let Some(out) = side_untracked.as_deref_mut() {
            out.extend(parallel_untracked);
        }
        for (entry_index, rel_path) in &ordered {
            let lookup = preloaded[*entry_index].take().ok_or_else(|| {
                Error::Message(format!(
                    "parallel blob stat preload missing index entry {entry_index}"
                ))
            })?;
            maybe_collect_file_to_directory_untracked(
                indexed_stage0_paths,
                indexed_blob_modes,
                work_tree_for_untracked,
                side_untracked.as_deref_mut(),
                rel_path,
                &lookup,
            )?;
            visit(*entry_index, rel_path, lookup)?;
        }
        return Ok(());
    }

    for_each_blob_by_directory(
        indexed_stage0_paths,
        indexed_blob_modes,
        work_tree_for_untracked,
        by_dir,
        |dir| dir_abs_arc(dir),
        |rel| file_abs_arc(rel),
        |rel| has_symlink_ancestor(rel),
        side_untracked,
        visit,
    )
}

fn lookup_blob_dir_entry(entry: &fs::DirEntry) -> BlobDiskLookup {
    match entry.file_type() {
        Ok(t) if t.is_symlink() => match symlink_metadata_path(&entry.path()) {
            Ok(m) => BlobDiskLookup::Present(m),
            Err(e) => BlobDiskLookup::Io(e),
        },
        Ok(_) => match dir_entry_metadata(entry) {
            Ok(m) => BlobDiskLookup::Present(m),
            Err(e) => BlobDiskLookup::Io(e),
        },
        Err(e) => BlobDiskLookup::Io(e),
    }
}

fn dir_entry_name_matches(index_file_name: &str, entry: &fs::DirEntry) -> bool {
    let disk = entry.file_name();
    let disk = disk.to_string_lossy();
    if disk == index_file_name {
        return true;
    }
    #[cfg(windows)]
    {
        disk.eq_ignore_ascii_case(index_file_name)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn lookup_blob_in_directory(
    on_disk: &HashMap<String, fs::DirEntry>,
    index_file_name: &str,
    target_abs: &Path,
) -> BlobDiskLookup {
    if let Some(entry) = on_disk.get(index_file_name) {
        return lookup_blob_dir_entry(entry);
    }
    for entry in on_disk.values() {
        if dir_entry_name_matches(index_file_name, entry) {
            return lookup_blob_dir_entry(entry);
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
    #[cfg(test)]
    if probe_counts(abs_dir) {
        WORKTREE_READ_DIR_PROBE.fetch_add(1, Ordering::Relaxed);
    }
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
        let grouped = group_blob_entries_by_dir(&index, false);
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
            None,
            None,
            None,
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            None,
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
    fn side_untracked_collects_sibling_beside_assume_unchanged() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "foo/tracked.txt", b"tracked\n");
        fs::write(wt.join("foo/sibling.txt"), b"untracked\n").unwrap();
        let mut index = Index::new();
        let mut entry = entry_from_stat(
            &wt.join("foo/tracked.txt"),
            b"foo/tracked.txt",
            oid,
            0o100644,
        )
        .unwrap();
        entry.set_assume_unchanged(true);
        index.add_or_replace(entry);
        let mut side = Vec::new();
        supplement_untracked_beside_stat_skipped_indexed_dirs(&index, wt, &mut side).unwrap();
        assert!(
            side.iter().any(|p| p == "foo/sibling.txt"),
            "expected untracked sibling beside assume-unchanged, got {side:?}"
        );
    }

    #[test]
    fn side_untracked_collects_sibling_in_single_blob_directory() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "d0/tracked.txt", b"tracked\n");
        fs::write(wt.join("d0/new.txt"), b"untracked\n").unwrap();
        let mut index = Index::new();
        index.add_or_replace(
            entry_from_stat(&wt.join("d0/tracked.txt"), b"d0/tracked.txt", oid, 0o100644).unwrap(),
        );
        let grouped = group_blob_entries_by_dir(&index, false);
        let paths = stage0_indexed_paths(&index);
        let mut side = Vec::new();
        for_each_blob_by_directory(
            Some(&paths),
            None,
            None,
            &grouped,
            |dir| {
                if dir.is_empty() {
                    wt.to_path_buf()
                } else {
                    wt.join(dir)
                }
            },
            |rel| wt.join(rel),
            |_| false,
            Some(&mut side),
            |_, _, _| Ok(()),
        )
        .unwrap();
        assert!(
            side.iter().any(|p| p == "d0/new.txt"),
            "expected untracked sibling, got {side:?}"
        );
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

        let grouped = group_blob_entries_by_dir(&index, false);
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
            None,
            None,
            None,
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            None,
            |_, _, lookup| {
                missing = matches!(lookup, BlobDiskLookup::Missing);
                Ok(())
            },
        )
        .unwrap();
        assert!(missing);
    }

    #[test]
    fn deep_linear_paths_stat_directly_without_read_dir() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        const CHAINS: usize = 120;
        const DEPTH: usize = 10;
        let mut index = Index::new();
        for chain in 0..CHAINS {
            let mut rel = format!("root{chain:03}");
            for d in 0..DEPTH {
                rel.push_str(&format!("/dir{d}"));
            }
            rel.push_str("/leaf.txt");
            let oid = write_blob_file(wt, &rel, b"payload\n");
            index.add_or_replace(
                entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
            );
        }
        let grouped = group_blob_entries_by_dir(&index, false);
        let _probe = reset_worktree_metadata_probe(wt);
        let dir_abs = |dir: &str| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                wt.join(dir)
            }
        };
        let file_abs = |rel: &str| wt.join(rel);
        let mut seen = 0;
        for_each_blob_by_directory(
            None,
            None,
            None,
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            None,
            |_, _, lookup| {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, CHAINS);
        assert_eq!(
            worktree_read_dir_probe_count(),
            0,
            "single-blob parent dirs must not read_dir"
        );
        assert!(
            worktree_metadata_probe_count() <= CHAINS + 5,
            "expected ~one stat per file, got {}",
            worktree_metadata_probe_count()
        );
    }

    #[test]
    fn parallel_scan_uses_read_dir_for_multi_file_directories() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().to_path_buf();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        const DIRS: usize = 450;
        const FILES_PER_DIR: usize = 3;
        let mut index = Index::new();
        for d in 0..DIRS {
            let dir = format!("leaf{d:05}");
            for f in 0..FILES_PER_DIR {
                let rel = format!("{dir}/part{f}.txt");
                let oid = write_blob_file(&wt, &rel, format!("{d}-{f}\n").as_bytes());
                index.add_or_replace(
                    entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
                );
            }
        }
        let grouped = group_blob_entries_by_dir(&index, false);
        let _probe = reset_worktree_metadata_probe(&wt);
        let wt_dir = wt.clone();
        let dir_abs = move |dir: &str| {
            if dir.is_empty() {
                wt_dir.clone()
            } else {
                wt_dir.join(dir)
            }
        };
        let wt_file = wt.clone();
        let file_abs = move |rel: &str| wt_file.join(rel);
        let policy = crate::parallel::ThreadPolicy::default();
        let config = crate::config::ConfigSet::new();
        let mut seen = 0;
        for_each_blob_by_directory_parallel(
            None,
            None,
            None,
            &grouped,
            WorktreeBlobScanOptions {
                preload_index: true,
                stat_parallel_threads: Some(4),
            },
            &policy,
            &config,
            false,
            dir_abs,
            file_abs,
            |_| false,
            None,
            |_, _, lookup| {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, DIRS * FILES_PER_DIR);
        assert!(
            worktree_read_dir_probe_count() >= DIRS,
            "parallel preload must read_dir each multi-file parent (got {} read_dirs)",
            worktree_read_dir_probe_count()
        );
        let tracked = DIRS * FILES_PER_DIR;
        assert!(
            worktree_metadata_probe_count() <= tracked + DIRS + 8,
            "metadata probes {} exceeded budget for grouped scan",
            worktree_metadata_probe_count()
        );
    }

    #[test]
    fn dense_multi_file_dirs_one_read_dir_per_parent() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().to_path_buf();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        const DIRS: usize = 80;
        const FILES_PER_DIR: usize = 50;
        let mut index = Index::new();
        for d in 0..DIRS {
            let dir = format!("p/a/b/leaf{d:04}");
            for f in 0..FILES_PER_DIR {
                let rel = format!("{dir}/f{f}-file.txt");
                let oid = write_blob_file(&wt, &rel, format!("{d}-{f}\n").as_bytes());
                index.add_or_replace(
                    entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
                );
            }
        }
        let grouped = group_blob_entries_by_dir(&index, false);
        let _probe = reset_worktree_metadata_probe(&wt);
        let wt_dir = wt.clone();
        let dir_abs = move |dir: &str| {
            if dir.is_empty() {
                wt_dir.clone()
            } else {
                wt_dir.join(dir)
            }
        };
        let wt_file = wt.clone();
        let file_abs = move |rel: &str| wt_file.join(rel);
        let policy = crate::parallel::ThreadPolicy::default();
        let config = crate::config::ConfigSet::new();
        let mut seen = 0;
        for_each_blob_by_directory_parallel(
            None,
            None,
            None,
            &grouped,
            WorktreeBlobScanOptions {
                preload_index: true,
                stat_parallel_threads: Some(4),
            },
            &policy,
            &config,
            false,
            dir_abs,
            file_abs,
            |_| false,
            None,
            |_, _, lookup| {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, DIRS * FILES_PER_DIR);
        assert_eq!(
            worktree_read_dir_probe_count(),
            DIRS,
            "each dense parent directory must be read once"
        );
        let tracked = DIRS * FILES_PER_DIR;
        assert!(
            worktree_metadata_probe_count() <= tracked + DIRS + 8,
            "metadata probes {} exceeded budget for grouped scan",
            worktree_metadata_probe_count()
        );
    }

    #[test]
    fn core_preload_index_false_uses_single_thread() {
        let threads = stat_scan_thread_count(
            &crate::parallel::ThreadPolicy::default(),
            &crate::config::ConfigSet::new(),
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
    fn direct_parallel_scan_detects_directory_replaced_by_symlink() {
        use crate::diff::{
            diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus,
        };

        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        const FILES: usize = 1005;
        let mut index = Index::new();
        for i in 0..FILES {
            let rel = format!("dir/f{i:04}.txt");
            let oid = write_blob_file(wt, &rel, b"x\n");
            index.add_or_replace(
                entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
            );
        }
        fs::remove_dir_all(wt.join("dir")).unwrap();
        let external = wt.join("external");
        fs::create_dir_all(&external).unwrap();
        for i in 0..FILES {
            fs::write(external.join(format!("f{i:04}.txt")), b"y\n").unwrap();
        }
        std::os::unix::fs::symlink(&external, wt.join("dir")).unwrap();

        let odb = Odb::new(&wt.join(".git/objects"));
        let (diffs, _) = diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            DiffIndexToWorktreeOptions {
                for_status: true,
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("diff");
        assert_eq!(diffs.len(), FILES);
        assert!(diffs.iter().all(|d| d.status == DiffStatus::Deleted));
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
