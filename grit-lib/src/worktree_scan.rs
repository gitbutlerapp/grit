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
static WORKTREE_METADATA_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Test hook: `read_dir` calls during directory-grouped blob scans.
static WORKTREE_READ_DIR_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Only syscalls under this root are counted, so tests scanning other worktrees in
/// parallel do not inflate a probing test's counts.
static WORKTREE_PROBE_ACTIVE: AtomicUsize = AtomicUsize::new(0);
// hygiene: test-only probe root for integration switch regressions
static WORKTREE_PROBE_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Serializes probing tests; each owns the counters while its scope is alive.
// hygiene: test-only probe serialization for integration switch regressions
static WORKTREE_PROBE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Keeps worktree syscall probing armed for one test; disarms on drop.
#[doc(hidden)]
pub struct WorktreeProbeScope {
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Drop for WorktreeProbeScope {
    fn drop(&mut self) {
        *WORKTREE_PROBE_ROOT
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        WORKTREE_PROBE_ACTIVE.store(0, Ordering::Release);
    }
}

/// Zero the probe counters and count only syscalls on paths under `root`.
#[doc(hidden)]
pub fn reset_worktree_metadata_probe(root: &Path) -> WorktreeProbeScope {
    let serial = WORKTREE_PROBE_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *WORKTREE_PROBE_ROOT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(root.to_path_buf());
    WORKTREE_METADATA_PROBE.store(0, Ordering::Relaxed);
    WORKTREE_READ_DIR_PROBE.store(0, Ordering::Relaxed);
    WORKTREE_PROBE_ACTIVE.store(1, Ordering::Release);
    WorktreeProbeScope { _serial: serial }
}

fn probe_counts(path: &Path) -> bool {
    if WORKTREE_PROBE_ACTIVE.load(Ordering::Acquire) == 0 {
        return false;
    }
    WORKTREE_PROBE_ROOT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        .is_some_and(|root| path.starts_with(root))
}

/// Returns metadata syscall probes recorded since [`reset_worktree_metadata_probe`].
#[doc(hidden)]
pub fn worktree_metadata_probe_count() -> usize {
    WORKTREE_METADATA_PROBE.load(Ordering::Relaxed)
}

/// Returns `read_dir` probes recorded since [`reset_worktree_metadata_probe`].
#[doc(hidden)]
pub fn worktree_read_dir_probe_count() -> usize {
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

/// Directory-grouped blob scan plan: index entry indices grouped by parent directory.
///
/// Built in one pass over the path-sorted index (or a sorted path subset) without a
/// per-directory hash map of owned path strings.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlobScanPlan {
    /// Parent directories in lexicographic order (empty string is the repository root).
    pub dirs: Vec<String>,
    /// For each [`Self::dirs`] element, a range into [`Self::indices`].
    pub dir_ranges: Vec<std::ops::Range<usize>>,
    /// [`Index::entries`] indices in visit order (sorted by parent dir, then path).
    pub indices: Vec<usize>,
}

impl BlobScanPlan {
    #[must_use]
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    fn push_entry(&mut self, dir: String, entry_index: usize) {
        if self.dirs.last().is_none_or(|d| d != &dir) {
            if !self.dirs.is_empty() {
                let end = self.indices.len();
                if let Some(range) = self.dir_ranges.last_mut() {
                    range.end = end;
                }
            }
            self.dirs.push(dir);
            self.dir_ranges.push(self.indices.len()..self.indices.len());
        }
        self.indices.push(entry_index);
        if let Some(range) = self.dir_ranges.last_mut() {
            range.end = self.indices.len();
        }
    }

    fn finish(mut self) -> Self {
        if !self.dirs.is_empty() {
            let end = self.indices.len();
            if let Some(range) = self.dir_ranges.last_mut() {
                range.end = end;
            }
        }
        self
    }
}

fn index_blob_scan_eligible(
    ie: &crate::index::IndexEntry,
    index: &Index,
    honor_fsmonitor_valid: bool,
) -> bool {
    if ie.stage() != 0 {
        return false;
    }
    if ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
        return false;
    }
    if ie.mode == MODE_GITLINK {
        return false;
    }
    if honor_fsmonitor_valid && index.fsmonitor_last_update.is_some() && ie.fsmonitor_valid() {
        return false;
    }
    true
}

fn entry_rel_path(index: &Index, entry_index: usize) -> Result<&str> {
    let ie = index
        .entries
        .get(entry_index)
        .ok_or_else(|| Error::Message(format!("index entry {entry_index} out of range")))?;
    std::str::from_utf8(&ie.path)
        .map_err(|_| Error::Message(format!("index entry {entry_index} path is not valid UTF-8")))
}

fn entry_file_name(index: &Index, entry_index: usize) -> Result<&str> {
    let rel = entry_rel_path(index, entry_index)?;
    Ok(rel.rsplit_once('/').map_or(rel, |(_, file)| file))
}

/// Group stage-0 blob index entries by parent directory for bulk `read_dir`.
#[must_use]
pub(crate) fn plan_blob_entries_by_dir(index: &Index, honor_fsmonitor_valid: bool) -> BlobScanPlan {
    let mut plan = BlobScanPlan {
        dirs: Vec::new(),
        dir_ranges: Vec::new(),
        indices: Vec::new(),
    };
    for (entry_index, ie) in index.entries.iter().enumerate() {
        if !index_blob_scan_eligible(ie, index, honor_fsmonitor_valid) {
            continue;
        }
        let Ok(rel_path) = std::str::from_utf8(&ie.path) else {
            continue;
        };
        let dir = rel_path
            .rsplit_once('/')
            .map_or(String::new(), |(dir, _)| dir.to_owned());
        plan.push_entry(dir, entry_index);
    }
    plan.finish()
}

/// Like [`plan_blob_entries_by_dir`], but only for repository-relative paths in `paths`.
#[must_use]
pub(crate) fn plan_blob_entries_by_dir_for_paths(
    index: &Index,
    paths: &BTreeSet<String>,
) -> BlobScanPlan {
    let mut plan = BlobScanPlan {
        dirs: Vec::new(),
        dir_ranges: Vec::new(),
        indices: Vec::new(),
    };
    for path in paths {
        let path_bytes = path.as_bytes();
        let Some(ie) = index.get(path_bytes, 0) else {
            continue;
        };
        if !index_blob_scan_eligible(ie, index, false) {
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
        let dir = path
            .rsplit_once('/')
            .map_or(String::new(), |(dir, _)| dir.to_owned());
        plan.push_entry(dir, entry_index);
    }
    plan.finish()
}

/// Group stage-0 blob index entries by parent directory for bulk `read_dir`.
#[must_use]
pub(crate) fn group_blob_entries_by_dir(
    index: &Index,
    honor_fsmonitor_valid: bool,
) -> BlobScanPlan {
    plan_blob_entries_by_dir(index, honor_fsmonitor_valid)
}

/// Like [`group_blob_entries_by_dir`], but only for repository-relative paths in `paths`.
#[must_use]
pub(crate) fn group_blob_entries_by_dir_for_paths(
    index: &Index,
    paths: &BTreeSet<String>,
) -> BlobScanPlan {
    plan_blob_entries_by_dir_for_paths(index, paths)
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

fn collect_blob_lookups_in_directory(
    index: &Index,
    dir: &str,
    entry_indices: &[usize],
    dir_abs: &dyn Fn(&str) -> PathBuf,
    file_abs: &dyn Fn(&str) -> PathBuf,
    symlink_missing: &HashSet<String>,
    out: &mut Vec<(usize, BlobDiskLookup)>,
) -> Result<()> {
    for_each_blob_in_one_directory(
        index,
        dir,
        entry_indices,
        dir_abs,
        file_abs,
        &mut |rel| symlink_missing.contains(rel),
        &mut |entry_index, _path, lookup| {
            out.push((entry_index, lookup));
            Ok(())
        },
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn for_each_blob_in_one_directory<F, H>(
    index: &Index,
    dir: &str,
    entry_indices: &[usize],
    dir_abs: &dyn Fn(&str) -> PathBuf,
    file_abs: &dyn Fn(&str) -> PathBuf,
    has_symlink_ancestor: &mut H,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(usize, &str, BlobDiskLookup) -> Result<()>,
    H: FnMut(&str) -> bool,
{
    if entry_indices.is_empty() {
        return Ok(());
    }
    if entry_indices.iter().all(|&i| match entry_rel_path(index, i) {
        Ok(rel) => has_symlink_ancestor(rel),
        Err(_) => true,
    }) {
        for &entry_index in entry_indices {
            let rel_path = entry_rel_path(index, entry_index)?;
            visit(entry_index, rel_path, BlobDiskLookup::Missing)?;
        }
        return Ok(());
    }

    if entry_indices.len() < READ_DIR_AMORTIZE_MIN {
        for &entry_index in entry_indices {
            let rel_path = entry_rel_path(index, entry_index)?;
            if has_symlink_ancestor(rel_path) {
                visit(entry_index, rel_path, BlobDiskLookup::Missing)?;
                continue;
            }
            let target = file_abs(rel_path);
            let lookup = match symlink_metadata_path(&target) {
                Ok(meta) => BlobDiskLookup::Present(meta),
                Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
                    BlobDiskLookup::Missing
                }
                Err(e) => return Err(Error::Io(e)),
            };
            visit(entry_index, rel_path, lookup)?;
        }
        return Ok(());
    }

    let abs_dir = dir_abs(dir);
    let (dir_readable, on_disk) = read_directory_map(&abs_dir)?;
    for &entry_index in entry_indices {
        let rel_path = entry_rel_path(index, entry_index)?;
        if has_symlink_ancestor(rel_path) {
            visit(entry_index, rel_path, BlobDiskLookup::Missing)?;
            continue;
        }
        let file_name = entry_file_name(index, entry_index)?;
        let target = file_abs(rel_path);
        let lookup = if !dir_readable {
            BlobDiskLookup::Missing
        } else {
            lookup_blob_in_directory(&on_disk, file_name, &target)
        };
        visit(entry_index, rel_path, lookup)?;
    }
    Ok(())
}

fn collect_blob_lookups_serial(
    index: &Index,
    plan: &BlobScanPlan,
    symlink_missing: &HashSet<String>,
    dir_abs: &dyn Fn(&str) -> PathBuf,
    file_abs: &dyn Fn(&str) -> PathBuf,
) -> Result<Vec<(usize, BlobDiskLookup)>> {
    let mut out = Vec::with_capacity(plan.len());
    for (dir_idx, dir) in plan.dirs.iter().enumerate() {
        let Some(range) = plan.dir_ranges.get(dir_idx) else {
            continue;
        };
        collect_blob_lookups_in_directory(
            index,
            dir,
            &plan.indices[range.clone()],
            dir_abs,
            file_abs,
            symlink_missing,
            &mut out,
        )?;
    }
    Ok(out)
}

fn preload_blob_lookups_parallel(
    index: &Index,
    plan: &BlobScanPlan,
    threads: usize,
    symlink_missing: &HashSet<String>,
    dir_abs: Arc<dyn Fn(&str) -> PathBuf + Send + Sync>,
    file_abs: Arc<dyn Fn(&str) -> PathBuf + Send + Sync>,
) -> Result<Vec<(usize, BlobDiskLookup)>> {
    let blob_count = plan.len();
    if plan.dirs.is_empty() {
        return Ok(Vec::new());
    }
    let chunk_size = plan.dirs.len().div_ceil(threads);
    let num_chunks = plan.dirs.len().div_ceil(chunk_size);
    let shared_plan = Arc::new(BlobScanPlan {
        dirs: plan.dirs.clone(),
        dir_ranges: plan.dir_ranges.clone(),
        indices: plan.indices.clone(),
    });
    let mut merged: Vec<(usize, BlobDiskLookup)> = Vec::with_capacity(blob_count);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk_idx in 0..num_chunks {
            let chunk_start = chunk_idx * chunk_size;
            let chunk_end = (chunk_start + chunk_size).min(shared_plan.dirs.len());
            let symlink_missing = symlink_missing.clone();
            let dir_abs = Arc::clone(&dir_abs);
            let file_abs = Arc::clone(&file_abs);
            let shared_plan = Arc::clone(&shared_plan);
            handles.push(
                scope.spawn(move || -> Result<Vec<(usize, BlobDiskLookup)>> {
                    let mut local = Vec::new();
                    for dir_idx in chunk_start..chunk_end {
                        let dir = &shared_plan.dirs[dir_idx];
                        let Some(range) = shared_plan.dir_ranges.get(dir_idx) else {
                            continue;
                        };
                        let entry_indices = &shared_plan.indices[range.clone()];
                        collect_blob_lookups_in_directory(
                            index,
                            dir,
                            entry_indices,
                            dir_abs.as_ref(),
                            file_abs.as_ref(),
                            &symlink_missing,
                            &mut local,
                        )?;
                    }
                    Ok(local)
                }),
            );
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(local)) => merged.extend(local),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Error::Message(
                        "parallel blob stat worker thread panicked".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    })?;
    if merged.len() != blob_count {
        return Err(Error::Message(format!(
            "parallel blob stat preload expected {blob_count} lookups, got {}",
            merged.len()
        )));
    }
    merged.sort_unstable_by_key(|(entry_index, _)| *entry_index);
    Ok(merged)
}

/// Visit grouped blob entries with one `read_dir` per distinct parent directory.
///
/// When `has_symlink_ancestor(rel_path)` is true, the entry is reported as [`BlobDiskLookup::Missing`]
/// without extra I/O (symlink replaced a directory on the path).
pub(crate) fn for_each_blob_by_directory<F, H>(
    index: &Index,
    plan: &BlobScanPlan,
    dir_abs: impl Fn(&str) -> PathBuf,
    file_abs: impl Fn(&str) -> PathBuf,
    mut has_symlink_ancestor: H,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, BlobDiskLookup) -> Result<()>,
    H: FnMut(&str) -> bool,
{
    let symlink_missing = build_symlink_missing(index, plan, &mut has_symlink_ancestor);
    let lookups = collect_blob_lookups_serial(index, plan, &symlink_missing, &dir_abs, &file_abs)?;
    for (entry_index, lookup) in lookups {
        visit(entry_index, lookup)?;
    }
    Ok(())
}

fn build_symlink_missing(
    index: &Index,
    plan: &BlobScanPlan,
    has_symlink_ancestor: &mut impl FnMut(&str) -> bool,
) -> HashSet<String> {
    let mut symlink_missing = HashSet::new();
    for (dir_idx, _) in plan.dirs.iter().enumerate() {
        let Some(range) = plan.dir_ranges.get(dir_idx) else {
            continue;
        };
        let Some(&sample_index) = plan.indices.get(range.start) else {
            continue;
        };
        let Ok(sample_path) = entry_rel_path(index, sample_index) else {
            continue;
        };
        if has_symlink_ancestor(sample_path) {
            for &entry_index in &plan.indices[range.clone()] {
                if let Ok(rel_path) = entry_rel_path(index, entry_index) {
                    symlink_missing.insert(rel_path.to_owned());
                }
            }
        }
    }
    symlink_missing
}

/// Collect disk metadata for every entry in `plan`, optionally using parallel preload.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_blob_lookups_for_plan(
    index: &Index,
    plan: &BlobScanPlan,
    scan_options: WorktreeBlobScanOptions,
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    scan_symlink_ancestors: bool,
    dir_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    mut has_symlink_ancestor: impl FnMut(&str) -> bool,
) -> Result<Vec<(usize, BlobDiskLookup)>> {
    let symlink_missing = if scan_symlink_ancestors {
        build_symlink_missing(index, plan, &mut has_symlink_ancestor)
    } else {
        HashSet::new()
    };

    let blob_count = plan.len();
    let threads = stat_scan_thread_count(thread_policy, config, scan_options, blob_count);
    let dir_abs_arc: Arc<dyn Fn(&str) -> PathBuf + Send + Sync> = Arc::new(dir_abs);
    let file_abs_arc: Arc<dyn Fn(&str) -> PathBuf + Send + Sync> = Arc::new(file_abs);
    if threads > 1 {
        // Always partition parallel preload by directory groups so multi-file parents use
        // one read_dir per directory (Git preload-index style). A flat per-file stat loop
        // regresses deep leaf-dir layouts (~50k paths, few files per directory).
        preload_blob_lookups_parallel(
            index,
            plan,
            threads,
            &symlink_missing,
            Arc::clone(&dir_abs_arc),
            Arc::clone(&file_abs_arc),
        )
    } else {
        collect_blob_lookups_serial(
            index,
            plan,
            &symlink_missing,
            dir_abs_arc.as_ref(),
            file_abs_arc.as_ref(),
        )
    }
}

/// Like [`for_each_blob_by_directory`], optionally preloading disk lookups in parallel.
#[allow(clippy::too_many_arguments)]
pub(crate) fn for_each_blob_by_directory_parallel<F>(
    index: &Index,
    plan: &BlobScanPlan,
    scan_options: WorktreeBlobScanOptions,
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    scan_symlink_ancestors: bool,
    dir_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    has_symlink_ancestor: impl FnMut(&str) -> bool,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, BlobDiskLookup) -> Result<()>,
{
    let lookups = collect_blob_lookups_for_plan(
        index,
        plan,
        scan_options,
        thread_policy,
        config,
        scan_symlink_ancestors,
        dir_abs,
        file_abs,
        has_symlink_ancestor,
    )?;
    for (entry_index, lookup) in lookups {
        visit(entry_index, lookup)?;
    }
    Ok(())
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
        return match dir_entry_metadata(entry) {
            Ok(m) => BlobDiskLookup::Present(m),
            Err(e) => BlobDiskLookup::Io(e),
        };
    }
    for entry in on_disk.values() {
        if dir_entry_name_matches(index_file_name, entry) {
            return match dir_entry_metadata(entry) {
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
        let plan = group_blob_entries_by_dir(&index, false);
        assert_eq!(plan.dirs.len(), 1);
        assert_eq!(plan.dirs[0], "d0");

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
            &index,
            &plan,
            dir_abs,
            file_abs,
            |_| false,
            |entry_index, lookup| {
                let path = std::str::from_utf8(&index.entries[entry_index].path).unwrap();
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

        let plan = group_blob_entries_by_dir(&index, false);
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
            &index,
            &plan,
            dir_abs,
            file_abs,
            |_| false,
            |_, lookup| {
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
        let plan = group_blob_entries_by_dir(&index, false);
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
            &index,
            &plan,
            dir_abs,
            file_abs,
            |_| false,
            |_, lookup| {
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
        let plan = group_blob_entries_by_dir(&index, false);
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
            &index,
            &plan,
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
            |_, lookup| {
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
        let plan = group_blob_entries_by_dir(&index, false);
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
            &index,
            &plan,
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
            |_, lookup| {
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
    fn parallel_unstaged_diff_detects_directory_replaced_by_symlink() {
        use crate::diff::{
            diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus,
        };
        use crate::parallel::ThreadPolicy;

        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let mut index = Index::new();
        let tracked_oid = write_blob_file(wt, "d/file.txt", b"same\n");
        index.add_or_replace(
            entry_from_stat(&wt.join("d/file.txt"), b"d/file.txt", tracked_oid, 0o100644).unwrap(),
        );
        const PADDING: usize = 1000;
        for i in 0..PADDING {
            let rel = format!("bulk/f{i:04}.txt");
            let oid = write_blob_file(wt, &rel, b"z\n");
            index.add_or_replace(
                entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
            );
        }
        fs::remove_dir_all(wt.join("d")).unwrap();
        let target = wt.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("file.txt"), b"same\n").unwrap();
        std::os::unix::fs::symlink(&target, wt.join("d")).unwrap();

        let odb = Odb::new(&wt.join(".git/objects"));
        let (diffs, _) = diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            DiffIndexToWorktreeOptions {
                thread_policy: Some(ThreadPolicy::default()),
                ..DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("diff");
        assert_eq!(
            diffs.len(),
            1,
            "unstaged parallel diff must report deletion at indexed path"
        );
        assert_eq!(diffs[0].status, DiffStatus::Deleted);
        assert_eq!(diffs[0].old_path.as_deref(), Some("d/file.txt"));
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
