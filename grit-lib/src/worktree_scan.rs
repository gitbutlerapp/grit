//! Directory-oriented worktree stat reads for index comparison.
//!
//! Groups indexed blob paths by parent directory. On Windows each directory is listed once with
//! `read_dir` and tracked files reuse the listing's `DirEntry` metadata, because a path stat
//! opens a handle per file there. Elsewhere each tracked path gets one metadata probe:
//! listing a directory costs more than probing its tracked files (`readdir` on ext4
//! dominates large directories). Literal paths on 64-bit Linux can share a parent handle
//! and use `fstatat` without following file symlinks, avoiding repeated absolute path
//! construction and lookup. Small directories and paths requiring case or Unicode
//! resolution keep the per-path metadata lookup.
//!
//! When [`WorktreeBlobScanOptions::preload_index`] is true and the tracked blob count is at
//! least [`PARALLEL_STAT_MIN_ENTRIES`], stat collection preloads disk lookups in parallel by
//! partitioning sorted index entries across a small thread pool (`std::thread::scope`). Cuts
//! prefer parent-directory boundaries unless one directory holds more entries than a fair
//! worker share (Git `preload-index` style). On Windows each parent directory stays on one
//! worker because listing is shared via `read_dir`. Visit order stays directory-sorted so diff
//! output matches the serial path; only the metadata probes are parallelized. A later shared
//! thread pool can replace the scoped helper without changing callers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::sync::mpsc::{self, RecvTimeoutError};

use crate::error::{Error, Result};
use crate::progress::{poll_watch, StatScanCancel, CANCEL_POLL_STRIDE};

type DirAbsFn = dyn Fn(&[u8]) -> PathBuf + Send + Sync;

#[derive(Clone, Copy)]
enum StatBoundaryPoll<'a> {
    Owner(&'a StatScanCancel),
    Worker(Option<&'a Arc<AtomicBool>>),
}

impl StatBoundaryPoll<'_> {
    fn poll_directory(&self) -> Result<()> {
        match self {
            Self::Owner(cancel) => cancel.poll(),
            Self::Worker(watch) => {
                if let Some(w) = watch {
                    poll_watch(w)?;
                }
                Ok(())
            }
        }
    }

    fn poll_stride(&self, counter: &mut usize) -> Result<()> {
        *counter = counter.wrapping_add(1);
        if !counter.is_multiple_of(CANCEL_POLL_STRIDE) {
            return Ok(());
        }
        match self {
            Self::Owner(cancel) => cancel.poll(),
            Self::Worker(watch) => {
                if let Some(w) = watch {
                    poll_watch(w)?;
                }
                Ok(())
            }
        }
    }
}

/// Whether tracked files are stat-ed through one `read_dir` of their parent (see module docs).
/// On NTFS, one listing per parent beats a path stat per indexed file even for single-file
/// leaf directories (issue #1231).
const LIST_PARENT_DIRECTORIES: bool = cfg!(windows);

/// Test hook: incremented once per worktree metadata syscall used by directory scans.
static WORKTREE_METADATA_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Test hook: `read_dir` calls during directory-grouped blob scans.
static WORKTREE_READ_DIR_PROBE: AtomicUsize = AtomicUsize::new(0);

/// Test hook: `core.ignorecase` worktree path resolutions that probe the filesystem.
static WORKTREE_PATH_RESOLVE_PROBE: AtomicUsize = AtomicUsize::new(0);

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
    WORKTREE_PATH_RESOLVE_PROBE.store(0, Ordering::Relaxed);
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
#[cfg(test)]
pub(crate) fn worktree_read_dir_probe_count() -> usize {
    WORKTREE_READ_DIR_PROBE.load(Ordering::Relaxed)
}

/// Returns `core.ignorecase` path resolutions recorded since [`reset_worktree_metadata_probe`].
#[doc(hidden)]
pub fn worktree_path_resolve_probe_count() -> usize {
    WORKTREE_PATH_RESOLVE_PROBE.load(Ordering::Relaxed)
}

/// Counts one filesystem-probing `core.ignorecase` path resolution for `path`.
pub(crate) fn probe_worktree_path_resolution(path: &Path) {
    if probe_counts(path) {
        WORKTREE_PATH_RESOLVE_PROBE.fetch_add(1, Ordering::Relaxed);
    }
}

/// Minimum tracked blob entries before parallel stat preload is considered.
pub const PARALLEL_STAT_MIN_ENTRIES: usize = 1000;

/// Minimum entries per parallel slice so Linux amortizes one `open_stat_directory` handle.
#[cfg(not(windows))]
const PARALLEL_STAT_MIN_SLICE_ENTRIES: usize = 8;

/// Parallelism controls for directory-grouped blob scans (honors `core.preloadIndex`).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WorktreeBlobScanOptions {
    /// When false, parallel stat workers are off unless
    /// [`Self::parallel_stat_without_preload`] is set (honors `core.preloadIndex` when true).
    pub preload_index: bool,
    /// When `Some(1)`, forces a serial scan. When `Some(n > 1)`, caps workers at `n`.
    /// When `None`, uses the thread policy helpers below.
    pub stat_parallel_threads: Option<usize>,
    /// Parallel directory stat scans without treating the scan as index preload (no
    /// `core.preloadIndex` gate); used when callers must keep RSS down but still stat widely.
    pub parallel_stat_without_preload: bool,
}

/// Returns how many threads a blob scan should use for `blob_count` entries.
#[must_use]
pub(crate) fn stat_scan_thread_count(
    policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    options: WorktreeBlobScanOptions,
    blob_count: usize,
) -> usize {
    if blob_count < PARALLEL_STAT_MIN_ENTRIES {
        return 1;
    }
    if options.parallel_stat_without_preload {
        return policy.index_stat_scan_threads(config, blob_count, options.stat_parallel_threads);
    }
    if !options.preload_index {
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
    if WORKTREE_PROBE_ACTIVE.load(Ordering::Acquire) != 0 {
        probe_worktree_metadata_call(&entry.path());
    }
    entry.metadata()
}

fn symlink_metadata_path(path: &Path) -> io::Result<fs::Metadata> {
    probe_worktree_metadata_call(path);
    fs::symlink_metadata(path)
}

/// What a worktree path is on disk, without following a final symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AncestorKind {
    /// A real directory.
    Directory,
    /// A symlink (or, on Windows, a reparse point classified as one).
    Symlink,
    /// Missing, unreadable, or not a directory.
    Other,
}

/// Classify `path` with one metadata query, without following a final symlink.
///
/// On Windows, `symlink_metadata` opens a handle per call, which dominates status on large
/// trees. A single attribute query classifies the common case (no reparse point) instead;
/// only reparse points fall back to `symlink_metadata` to classify the reparse tag.
#[must_use]
pub(crate) fn ancestor_kind(path: &Path) -> AncestorKind {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileAttributesW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            INVALID_FILE_ATTRIBUTES,
        };
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if !wide.contains(&0) {
            wide.push(0);
            // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
            let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
            if attrs == INVALID_FILE_ATTRIBUTES {
                return AncestorKind::Other;
            }
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                return if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    AncestorKind::Directory
                } else {
                    AncestorKind::Other
                };
            }
        }
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => AncestorKind::Symlink,
        Ok(meta) if meta.is_dir() => AncestorKind::Directory,
        _ => AncestorKind::Other,
    }
}

#[cfg(windows)]
fn sparse_stat_policy(sparse: BlobScanSparseMode) -> crate::index::StatMatchPolicy {
    match sparse {
        BlobScanSparseMode::StatusDiff { stat_policy, .. }
        | BlobScanSparseMode::WorktreeGuard { stat_policy, .. }
        | BlobScanSparseMode::StatRefresh { stat_policy, .. } => stat_policy,
        _ => crate::index::StatMatchPolicy::default(),
    }
}

/// When a directory scan stat disagrees with the index but path `symlink_metadata` agrees,
/// prefer the path probe (Git `mingw_stat` / post-close semantics on Windows).
///
/// `target_abs` is only called for that fallback probe: resolving the indexed path can cost
/// its own probe under `core.ignorecase`, which clean entries must not pay (#2167).
#[cfg(windows)]
fn align_lookup_stat_with_index_path_windows(
    index: &Index,
    entry_index: usize,
    lookup: BlobDiskLookup,
    target_abs: impl FnOnce() -> PathBuf,
    stat_policy: crate::index::StatMatchPolicy,
) -> BlobDiskLookup {
    let BlobDiskLookup::Present(stat) = lookup else {
        return lookup;
    };
    let ie = &index.entries[entry_index];
    if crate::diff::stat_matches_blob_stat(ie, &stat, stat_policy) {
        return BlobDiskLookup::Present(stat);
    }
    // NTFS read_dir metadata can disagree with the index on ctime/inode while mtime+size still
    // match; avoid a second path stat in that case (#1622). A disagreement in the sub-second
    // fractions compared under `core.usenanosec` still takes the path probe, or every such
    // entry would be re-hashed.
    if stat.fields.size_and_times_match_index(ie, stat_policy) {
        return BlobDiskLookup::Present(stat);
    }
    match symlink_metadata_path(&target_abs()) {
        Ok(meta) => {
            let path_stat = BlobDiskStat::from_metadata(&meta);
            if crate::diff::stat_matches_blob_stat(ie, &path_stat, stat_policy) {
                BlobDiskLookup::Present(path_stat)
            } else {
                BlobDiskLookup::Present(stat)
            }
        }
        // `read_dir` already found the file; keep the scan stat when the path probe fails.
        Err(_) => BlobDiskLookup::Present(stat),
    }
}

use std::collections::BTreeSet;

use crate::index::{Index, MODE_GITLINK};

/// Directory-grouped blob scan plan: index entry indices grouped by parent directory.
///
/// Built in one pass over the path-sorted index (or a sorted path subset) without a
/// per-directory hash map of owned path strings.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlobScanPlan {
    /// Parent directories in lexicographic order (empty is the repository root).
    pub dirs: Vec<Vec<u8>>,
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

    fn push_entry(&mut self, dir: &[u8], entry_index: usize) {
        if self.dirs.last().is_none_or(|d| d.as_slice() != dir) {
            if !self.dirs.is_empty() {
                let end = self.indices.len();
                if let Some(range) = self.dir_ranges.last_mut() {
                    range.end = end;
                }
            }
            self.dirs.push(dir.to_vec());
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

fn dir_entry_key(entry: &fs::DirEntry) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        entry.file_name().into_vec()
    }
    #[cfg(not(unix))]
    {
        entry
            .file_name()
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    }
}

fn entry_rel_path(index: &Index, entry_index: usize) -> Result<&[u8]> {
    index
        .entries
        .get(entry_index)
        .map(|ie| ie.path.as_slice())
        .ok_or_else(|| Error::Message(format!("index entry {entry_index} out of range")))
}

fn entry_file_name(index: &Index, entry_index: usize) -> Result<&[u8]> {
    let rel = entry_rel_path(index, entry_index)?;
    Ok(crate::repo_relpath::file_name(rel))
}

/// Borrowed form of [`crate::repo_relpath::parent_dir`].
fn parent_dir_bytes(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(pos) => &path[..pos],
        None => &[],
    }
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
        plan.push_entry(parent_dir_bytes(&ie.path), entry_index);
    }
    plan.finish()
}

/// Like [`plan_blob_entries_by_dir`], but only for the given index entry positions.
#[must_use]
pub(crate) fn plan_blob_entries_by_dir_for_positions(
    index: &Index,
    positions: &[usize],
    honor_fsmonitor_valid: bool,
) -> BlobScanPlan {
    let mut plan = BlobScanPlan {
        dirs: Vec::new(),
        dir_ranges: Vec::new(),
        indices: Vec::new(),
    };
    for &entry_index in positions {
        let Some(ie) = index.entries.get(entry_index) else {
            continue;
        };
        if !index_blob_scan_eligible(ie, index, honor_fsmonitor_valid) {
            continue;
        }
        plan.push_entry(parent_dir_bytes(&ie.path), entry_index);
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
        let path_bytes = crate::repo_relpath::storage_bytes(path);
        let Some(ie) = index.get(path_bytes.as_ref(), 0) else {
            continue;
        };
        if !index_blob_scan_eligible(ie, index, false) {
            continue;
        }
        let path_bytes_ref = path_bytes.as_ref();
        let Ok(entry_index) = index.entries.binary_search_by(|e| {
            e.path
                .as_slice()
                .cmp(path_bytes_ref)
                .then_with(|| e.stage().cmp(&0))
        }) else {
            continue;
        };
        plan.push_entry(parent_dir_bytes(path_bytes_ref), entry_index);
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

/// Compact worktree stat captured during a directory-grouped blob scan.
///
/// Holds the index-relevant stat tuple without retaining full [`fs::Metadata`] for every
/// tracked path (large status/diff scans).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlobDiskStat {
    pub(crate) fields: crate::index::IndexStatFields,
    pub(crate) git_mode: u32,
    pub(crate) is_dir: bool,
    /// Worktree byte length before index munging (`st_size` / `metadata.len()`).
    pub(crate) raw_size: u64,
}

impl BlobDiskStat {
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    fn from_linux_stat(stat: &libc::stat) -> Self {
        let kind = stat.st_mode & libc::S_IFMT;
        Self {
            fields: crate::index::IndexStatFields::from_linux_stat(stat),
            git_mode: if kind == libc::S_IFLNK {
                crate::index::MODE_SYMLINK
            } else if stat.st_mode & 0o111 != 0 {
                crate::index::MODE_EXECUTABLE
            } else {
                crate::index::MODE_REGULAR
            },
            is_dir: kind == libc::S_IFDIR,
            raw_size: stat.st_size as u64,
        }
    }

    #[must_use]
    pub(crate) fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            fields: crate::index::IndexStatFields::from_metadata(meta),
            git_mode: crate::diff::mode_from_metadata(meta),
            is_dir: meta.is_dir() && !meta.file_type().is_symlink(),
            raw_size: meta.len(),
        }
    }

    #[must_use]
    pub(crate) fn worktree_size(&self) -> u32 {
        self.fields.index_size()
    }
}

/// Result of looking up one indexed blob on disk during a directory scan.
pub(crate) enum BlobDiskLookup {
    /// Stat fields for the indexed path.
    Present(BlobDiskStat),
    /// Missing path or non-directory parent (`NotFound` / `ENOTDIR`).
    Missing,
    /// Other I/O error.
    Io(io::Error),
}

/// Controls whether a scanned lookup is kept in parallel preload batches.
///
/// Dense mode retains every entry (tests and callers that visit all paths). Sparse modes
/// keep only missing paths, errors, and entries that still need hash, refresh, or staging work.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BlobScanSparseMode {
    Dense,
    StatusDiff {
        stat_policy: crate::index::StatMatchPolicy,
        index_mtime: Option<(u32, u32)>,
        mode_policy: crate::index::WorktreeModePolicy,
        for_status: bool,
        refresh_index_stat_in_pass: bool,
        parallel_blob_work: bool,
        honor_fsmonitor_stat_skip: bool,
    },
    StageTracked {
        index_mtime: Option<(u32, u32)>,
        mode_policy: crate::index::WorktreeModePolicy,
    },
    WorktreeGuard {
        stat_policy: crate::index::StatMatchPolicy,
        index_mtime: Option<(u32, u32)>,
    },
    StatRefresh {
        stat_policy: crate::index::StatMatchPolicy,
        index_mtime: Option<(u32, u32)>,
    },
}

impl BlobScanSparseMode {
    fn retain_lookup(self, index: &Index, entry_index: usize, lookup: &BlobDiskLookup) -> bool {
        match self {
            Self::Dense => true,
            Self::StatusDiff {
                stat_policy,
                index_mtime,
                mode_policy,
                for_status,
                refresh_index_stat_in_pass,
                parallel_blob_work,
                honor_fsmonitor_stat_skip,
            } => retain_status_diff_lookup(
                index,
                entry_index,
                lookup,
                stat_policy,
                index_mtime,
                mode_policy,
                for_status,
                refresh_index_stat_in_pass,
                parallel_blob_work,
                honor_fsmonitor_stat_skip,
            ),
            Self::StageTracked {
                index_mtime,
                mode_policy,
            } => retain_stage_tracked_lookup(index, entry_index, lookup, index_mtime, mode_policy),
            Self::WorktreeGuard {
                stat_policy,
                index_mtime,
            } => retain_worktree_guard_lookup(index, entry_index, lookup, stat_policy, index_mtime),
            Self::StatRefresh {
                stat_policy,
                index_mtime,
            } => retain_stat_refresh_lookup(index, entry_index, lookup, stat_policy, index_mtime),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn retain_status_diff_lookup(
    index: &Index,
    entry_index: usize,
    lookup: &BlobDiskLookup,
    stat_policy: crate::index::StatMatchPolicy,
    index_mtime: Option<(u32, u32)>,
    mode_policy: crate::index::WorktreeModePolicy,
    for_status: bool,
    refresh_index_stat_in_pass: bool,
    parallel_blob_work: bool,
    honor_fsmonitor_stat_skip: bool,
) -> bool {
    let ie = &index.entries[entry_index];
    match lookup {
        BlobDiskLookup::Missing | BlobDiskLookup::Io(_) => true,
        BlobDiskLookup::Present(stat) => {
            if honor_fsmonitor_stat_skip && crate::fsmonitor::entry_skips_worktree_stat(ie, index) {
                return false;
            }
            if refresh_index_stat_in_pass
                && crate::diff::worktree_diff_refreshes_entry(
                    ie,
                    stat,
                    index_mtime,
                    stat_policy,
                    for_status,
                    // Attributes are not resolved during the scan; a changed size is kept
                    // either way, so assume a conversion and let the visit pass decide.
                    || false,
                )
            {
                return true;
            }
            if parallel_blob_work {
                // Keep everything the visit pass must hash or report; drop only entries whose
                // stat data proves them clean (the same test the visit pass applies).
                return !(crate::diff::stat_matches_blob_stat(ie, stat, stat_policy)
                    && mode_policy.worktree_mode_matches(stat.git_mode, ie.mode)
                    && crate::diff::index_entry_skips_worktree_content_hash_after_stat_match(
                        ie,
                        index_mtime,
                        stat_policy,
                    ));
            }
            true
        }
    }
}

fn retain_stage_tracked_lookup(
    index: &Index,
    entry_index: usize,
    lookup: &BlobDiskLookup,
    index_mtime: Option<(u32, u32)>,
    mode_policy: crate::index::WorktreeModePolicy,
) -> bool {
    match lookup {
        BlobDiskLookup::Missing | BlobDiskLookup::Io(_) => true,
        BlobDiskLookup::Present(stat) => {
            let ie = &index.entries[entry_index];
            if ie.mode == crate::index::MODE_GITLINK {
                return true;
            }
            let stat_same = stat
                .fields
                .matches_index_entry(ie, crate::index::StatMatchPolicy::default());
            let racy = crate::diff::entry_is_racy(ie, index_mtime);
            if !ie.intent_to_add()
                && ie.size != 0
                && stat_same
                && !racy
                && mode_policy.worktree_mode_matches(stat.git_mode, ie.mode)
            {
                return false;
            }
            true
        }
    }
}

fn retain_worktree_guard_lookup(
    index: &Index,
    entry_index: usize,
    lookup: &BlobDiskLookup,
    stat_policy: crate::index::StatMatchPolicy,
    index_mtime: Option<(u32, u32)>,
) -> bool {
    let ie = &index.entries[entry_index];
    if ie.stage() != 0 || ie.intent_to_add() || ie.skip_worktree() {
        return false;
    }
    match lookup {
        BlobDiskLookup::Missing => true,
        BlobDiskLookup::Io(_) => true,
        BlobDiskLookup::Present(stat) => {
            if ie.mode == crate::index::MODE_GITLINK {
                return !stat.is_dir;
            }
            if stat.is_dir
                || stat.git_mode != crate::index::MODE_SYMLINK
                    && stat.git_mode != crate::index::MODE_REGULAR
                    && stat.git_mode != crate::index::MODE_EXECUTABLE
            {
                return true;
            }
            // Racily clean entries are kept too: the guard must compare their content.
            !crate::diff::stat_matches_blob_stat(ie, stat, stat_policy)
                || crate::diff::entry_is_racy_with_policy(ie, index_mtime, stat_policy)
        }
    }
}

fn retain_stat_refresh_lookup(
    index: &Index,
    entry_index: usize,
    lookup: &BlobDiskLookup,
    stat_policy: crate::index::StatMatchPolicy,
    index_mtime: Option<(u32, u32)>,
) -> bool {
    let ie = &index.entries[entry_index];
    if ie.stage() != 0 || ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
        return false;
    }
    match lookup {
        BlobDiskLookup::Missing | BlobDiskLookup::Io(_) => false,
        BlobDiskLookup::Present(stat) => {
            !crate::diff::stat_matches_blob_stat(ie, stat, stat_policy)
                || crate::diff::entry_is_racy_with_policy(ie, index_mtime, stat_policy)
        }
    }
}

struct BlobLookupCollectSink<'a> {
    out: &'a mut Vec<(usize, BlobDiskLookup)>,
    sparse: BlobScanSparseMode,
    scanned: &'a mut usize,
}

impl BlobLookupCollectSink<'_> {
    fn push(&mut self, index: &Index, entry_index: usize, lookup: BlobDiskLookup) -> Result<()> {
        *self.scanned += 1;
        if self.sparse.retain_lookup(index, entry_index, &lookup) {
            self.out.push((entry_index, lookup));
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_blob_lookups_in_directory(
    index: &Index,
    dir: &[u8],
    entry_indices: &[usize],
    dir_abs: &dyn Fn(&[u8]) -> PathBuf,
    file_abs: &dyn Fn(&[u8]) -> PathBuf,
    literal_paths: bool,
    under_symlink: bool,
    out: &mut Vec<(usize, BlobDiskLookup)>,
    sparse: BlobScanSparseMode,
    scanned: &mut usize,
    boundary: StatBoundaryPoll<'_>,
    cancel_stride: &mut usize,
) -> Result<()> {
    let mut sink = BlobLookupCollectSink {
        out,
        sparse,
        scanned,
    };
    for_each_blob_in_one_directory(
        index,
        dir,
        entry_indices,
        dir_abs,
        file_abs,
        literal_paths,
        under_symlink,
        boundary,
        cancel_stride,
        &mut |entry_index, rel_path, lookup| {
            #[cfg(not(windows))]
            let _ = rel_path;
            #[cfg(windows)]
            let lookup = align_lookup_stat_with_index_path_windows(
                index,
                entry_index,
                lookup,
                || file_abs(rel_path),
                sparse_stat_policy(sparse),
            );
            sink.push(index, entry_index, lookup)
        },
    )?;
    Ok(())
}

/// Whether the directory holding `entry_indices` sits under a symlinked ancestor.
///
/// Every entry in one plan directory shares the same ancestors, so one sample path decides.
fn dir_has_symlink_ancestor(
    index: &Index,
    entry_indices: &[usize],
    has_symlink_ancestor: &mut dyn FnMut(&[u8]) -> bool,
) -> bool {
    entry_indices
        .first()
        .and_then(|&i| entry_rel_path(index, i).ok())
        .is_some_and(has_symlink_ancestor)
}

/// `under_symlink` reports every entry as [`BlobDiskLookup::Missing`] without I/O: a symlink
/// replaced a directory on the path, so the indexed files are gone even if the link target
/// holds files with the same names.
#[allow(clippy::too_many_arguments)]
fn for_each_blob_in_one_directory<F>(
    index: &Index,
    dir: &[u8],
    entry_indices: &[usize],
    dir_abs: &dyn Fn(&[u8]) -> PathBuf,
    file_abs: &dyn Fn(&[u8]) -> PathBuf,
    literal_paths: bool,
    under_symlink: bool,
    boundary: StatBoundaryPoll<'_>,
    cancel_stride: &mut usize,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(usize, &[u8], BlobDiskLookup) -> Result<()>,
{
    if under_symlink {
        for &entry_index in entry_indices {
            boundary.poll_stride(cancel_stride)?;
            let rel_path = entry_rel_path(index, entry_index)?;
            visit(entry_index, rel_path, BlobDiskLookup::Missing)?;
        }
        return Ok(());
    }

    #[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
    let _ = literal_paths;
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    // Amortize opening and closing a parent across several file lookups.
    if literal_paths && entry_indices.len() >= 8 {
        let abs_dir = dir_abs(dir);
        if let Ok(handle) = open_stat_directory(&abs_dir) {
            return for_each_blob_at_directory(
                index,
                entry_indices,
                &handle,
                &abs_dir,
                boundary,
                cancel_stride,
                visit,
            );
        }
    }

    if !LIST_PARENT_DIRECTORIES {
        // `dir_abs` already resolved the parent's case/Unicode spelling, so the indexed name
        // under it is the path `file_abs` would pick whenever it exists. Only a miss pays for
        // `file_abs`, which may probe and list the directory to find another spelling.
        let abs_dir = dir_abs(dir);
        for &entry_index in entry_indices {
            boundary.poll_stride(cancel_stride)?;
            let rel_path = entry_rel_path(index, entry_index)?;
            let file_name = entry_file_name(index, entry_index)?;
            let direct = crate::index::worktree_path_from_index_rel(&abs_dir, file_name);
            let lookup = match symlink_metadata_blob_lookup(&direct) {
                BlobDiskLookup::Missing => {
                    let target = file_abs(rel_path);
                    if target == direct {
                        BlobDiskLookup::Missing
                    } else {
                        symlink_metadata_blob_lookup(&target)
                    }
                }
                lookup => lookup,
            };
            visit(entry_index, rel_path, lookup)?;
        }
        return Ok(());
    }

    let abs_dir = dir_abs(dir);
    #[cfg(windows)]
    if entry_indices.len() == 1 {
        boundary.poll_stride(cancel_stride)?;
        let entry_index = entry_indices[0];
        let rel_path = entry_rel_path(index, entry_index)?;
        let file_name = entry_file_name(index, entry_index)?;
        let lookup = lookup_lone_blob_in_directory(&abs_dir, file_name, || file_abs(rel_path));
        visit(entry_index, rel_path, lookup)?;
        return Ok(());
    }

    match read_directory_map(&abs_dir, entry_indices.len())? {
        DirectoryReadOutcome::Listed(on_disk) => {
            for &entry_index in entry_indices {
                boundary.poll_stride(cancel_stride)?;
                let rel_path = entry_rel_path(index, entry_index)?;
                let file_name = entry_file_name(index, entry_index)?;
                let lookup = lookup_blob_in_directory(&on_disk, file_name, || file_abs(rel_path));
                visit(entry_index, rel_path, lookup)?;
            }
        }
        DirectoryReadOutcome::Missing => {
            for &entry_index in entry_indices {
                boundary.poll_stride(cancel_stride)?;
                let rel_path = entry_rel_path(index, entry_index)?;
                visit(entry_index, rel_path, BlobDiskLookup::Missing)?;
            }
        }
        DirectoryReadOutcome::Inaccessible(err) => {
            let kind = err.kind();
            let detail = err.to_string();
            for &entry_index in entry_indices {
                boundary.poll_stride(cancel_stride)?;
                let rel_path = entry_rel_path(index, entry_index)?;
                visit(
                    entry_index,
                    rel_path,
                    BlobDiskLookup::Io(io::Error::new(kind, detail.clone())),
                )?;
            }
        }
    }
    Ok(())
}

/// O_PATH permits metadata lookups in searchable directories without requiring read access.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn open_stat_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Confirms in one path walk that every component of a work-tree-relative directory is a
/// real directory, using `openat2` with `RESOLVE_NO_SYMLINKS` from a handle on the work tree.
///
/// Probing each ancestor with its own `lstat` walks the path from the root every time, which
/// is quadratic in depth. This answers the common case (no symlink, nothing missing) with one
/// walk; any other outcome is left to a per-component probe.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub(crate) struct RealDirChain {
    work_tree: PathBuf,
    /// `None` when the work tree cannot be opened or the kernel lacks `openat2`.
    root: Option<fs::File>,
    /// Reused NUL-terminated copy of the queried path.
    c_path: Vec<u8>,
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
impl RealDirChain {
    /// Open a handle on `work_tree` (following symlinks in the work tree's own path).
    pub(crate) fn open(work_tree: &Path) -> Self {
        use std::os::unix::fs::OpenOptionsExt;
        let root = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(work_tree)
            .ok();
        Self {
            work_tree: work_tree.to_path_buf(),
            root,
            c_path: Vec::new(),
        }
    }

    /// The work tree this handle was opened on.
    pub(crate) fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    /// Whether every component of `rel_dir` (raw index bytes) is known to be a real directory.
    ///
    /// `false` means "not confirmed": a component is a symlink, missing or not a directory, or
    /// the check is unavailable. Callers then probe component by component.
    pub(crate) fn confirms(&mut self, rel_dir: &[u8]) -> bool {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let Some(root) = &self.root else {
            return false;
        };
        if rel_dir.is_empty() || rel_dir.contains(&0) {
            return false;
        }
        self.c_path.clear();
        self.c_path.extend_from_slice(rel_dir);
        self.c_path.push(0);
        // SAFETY: `open_how` is plain integers, for which all-zero is valid (and means "no
        // flags, no mode, no resolve restrictions" to the kernel).
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64;
        how.resolve = libc::RESOLVE_NO_SYMLINKS;
        // SAFETY: `root` is an open descriptor, `c_path` is NUL-terminated and outlives the
        // call, and `how` is a fully initialized `open_how` whose size is passed alongside.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                self.c_path.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            let Ok(fd) = i32::try_from(fd) else {
                return false;
            };
            // SAFETY: the kernel returned a new descriptor that nothing else owns.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            return true;
        }
        let errno = io::Error::last_os_error().raw_os_error();
        if matches!(errno, Some(libc::ENOSYS | libc::EPERM | libc::E2BIG)) {
            self.root = None;
        }
        false
    }
}

/// Resolve each file relative to an already opened parent, without following file symlinks.
#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn for_each_blob_at_directory<F>(
    index: &Index,
    entry_indices: &[usize],
    directory: &fs::File,
    abs_dir: &Path,
    boundary: StatBoundaryPoll<'_>,
    cancel_stride: &mut usize,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(usize, &[u8], BlobDiskLookup) -> Result<()>,
{
    use std::os::fd::AsRawFd;
    let count_probes = probe_counts(abs_dir);
    let mut name = Vec::new();
    for &entry_index in entry_indices {
        boundary.poll_stride(cancel_stride)?;
        let rel = entry_rel_path(index, entry_index)?;
        let file_name = entry_file_name(index, entry_index)?;
        if file_name.contains(&0) {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file name contains NUL",
            )));
        }
        name.clear();
        name.reserve(file_name.len() + 1);
        name.extend_from_slice(file_name);
        name.push(0);
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if count_probes {
            WORKTREE_METADATA_PROBE.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: directory owns a valid fd; name is NUL-terminated and stat points to
        // writable storage. AT_SYMLINK_NOFOLLOW preserves lstat semantics for the file.
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr().cast(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let lookup = if result == 0 {
            // SAFETY: a successful fstatat initialized the entire stat structure.
            let stat = unsafe { stat.assume_init() };
            BlobDiskLookup::Present(BlobDiskStat::from_linux_stat(&stat))
        } else {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => BlobDiskLookup::Missing,
                _ => BlobDiskLookup::Io(error),
            }
        };
        visit(entry_index, rel, lookup)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_blob_lookups_serial(
    index: &Index,
    plan: &BlobScanPlan,
    has_symlink_ancestor: &mut dyn FnMut(&[u8]) -> bool,
    dir_abs: &dyn Fn(&[u8]) -> PathBuf,
    file_abs: &dyn Fn(&[u8]) -> PathBuf,
    literal_paths: bool,
    sparse: BlobScanSparseMode,
    cancel: &StatScanCancel,
) -> Result<(Vec<(usize, BlobDiskLookup)>, usize)> {
    let mut out = match sparse {
        BlobScanSparseMode::Dense => Vec::with_capacity(plan.len()),
        _ => Vec::new(),
    };
    let mut scanned = 0usize;
    let mut cancel_stride = 0usize;
    let boundary = StatBoundaryPoll::Owner(cancel);
    for (dir_idx, dir) in plan.dirs.iter().enumerate() {
        boundary.poll_directory()?;
        let Some(range) = plan.dir_ranges.get(dir_idx) else {
            continue;
        };
        let entry_indices = &plan.indices[range.clone()];
        let under_symlink = dir_has_symlink_ancestor(index, entry_indices, has_symlink_ancestor);
        collect_blob_lookups_in_directory(
            index,
            dir,
            entry_indices,
            dir_abs,
            file_abs,
            literal_paths,
            under_symlink,
            &mut out,
            sparse,
            &mut scanned,
            boundary,
            &mut cancel_stride,
        )?;
    }
    Ok((out, scanned))
}

/// Apply a visit callback to lookup batches produced by [`collect_blob_lookup_batches`].
pub(crate) fn visit_blob_lookup_batches<F>(
    batches: Vec<Vec<(usize, BlobDiskLookup)>>,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, BlobDiskLookup) -> Result<()>,
{
    for batch in batches {
        for (entry_index, lookup) in batch {
            visit(entry_index, lookup)?;
        }
    }
    Ok(())
}

/// One indexed file under `abs_dir`: a single `read_dir` without building a map (Windows leaf dirs).
#[cfg(windows)]
fn lookup_lone_blob_in_directory(
    abs_dir: &Path,
    index_file_name: &[u8],
    target_abs: impl FnOnce() -> PathBuf,
) -> BlobDiskLookup {
    if probe_counts(abs_dir) {
        WORKTREE_READ_DIR_PROBE.fetch_add(1, Ordering::Relaxed);
    }
    let rd = match fs::read_dir(abs_dir) {
        Ok(rd) => rd,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return BlobDiskLookup::Missing;
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return BlobDiskLookup::Io(e),
        Err(e) => return BlobDiskLookup::Io(e),
    };
    for item in rd {
        let Ok(entry) = item else { continue };
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        if dir_entry_name_matches(index_file_name, &entry) {
            return blob_lookup_from_dir_entry_or_path(Some(&entry), target_abs);
        }
    }
    symlink_metadata_blob_lookup(&target_abs())
}

#[cfg(not(windows))]
fn dir_range_for_index_pos(plan: &BlobScanPlan, index_pos: usize) -> std::ops::Range<usize> {
    for range in &plan.dir_ranges {
        if range.contains(&index_pos) {
            return range.clone();
        }
    }
    index_pos..index_pos.saturating_add(1)
}

fn first_dir_index_at_or_after(plan: &BlobScanPlan, index_pos: usize) -> usize {
    plan.dir_ranges
        .partition_point(|range| range.end <= index_pos)
        .min(plan.dirs.len().saturating_sub(1))
}

/// Move an index cut off a small directory interior onto the nearest directory boundary.
#[cfg(not(windows))]
fn snap_index_parallel_cut(plan: &BlobScanPlan, cut: usize, fair_share: usize) -> usize {
    if cut == 0 || cut >= plan.len() {
        return cut;
    }
    let dir_range = dir_range_for_index_pos(plan, cut - 1);
    if cut <= dir_range.start || cut >= dir_range.end {
        return cut;
    }
    let dir_len = dir_range.end - dir_range.start;
    if dir_len > fair_share {
        return cut;
    }
    if cut - dir_range.start <= dir_range.end - cut {
        dir_range.start
    } else {
        dir_range.end
    }
}

#[cfg(windows)]
fn partition_blob_lookup_slices(
    plan: &BlobScanPlan,
    threads: usize,
) -> Vec<std::ops::Range<usize>> {
    let num_dirs = plan.dirs.len();
    if num_dirs == 0 {
        return Vec::new();
    }
    if threads <= 1 {
        let n = plan.len();
        return if n == 0 {
            Vec::new()
        } else {
            std::iter::once(0..n).collect()
        };
    }
    let chunk_size = num_dirs.div_ceil(threads);
    let num_chunks = num_dirs.div_ceil(chunk_size);
    (0..num_chunks)
        .map(|chunk_idx| {
            let chunk_start = chunk_idx * chunk_size;
            let chunk_end = (chunk_start + chunk_size).min(num_dirs);
            let start = plan.dir_ranges[chunk_start].start;
            let end = plan.dir_ranges[chunk_end - 1].end;
            start..end
        })
        .collect()
}

#[cfg(not(windows))]
fn partition_blob_lookup_slices(
    plan: &BlobScanPlan,
    threads: usize,
) -> Vec<std::ops::Range<usize>> {
    let n = plan.len();
    if n == 0 {
        return Vec::new();
    }
    if threads <= 1 {
        return if n == 0 {
            Vec::new()
        } else {
            std::iter::once(0..n).collect()
        };
    }
    let fair_share = n.div_ceil(threads);
    let mut cuts = vec![0usize];
    for worker in 1..threads {
        let mut cut = worker * n / threads;
        cut = snap_index_parallel_cut(plan, cut, fair_share);
        let prev = cuts[cuts.len() - 1];
        let min_step = PARALLEL_STAT_MIN_SLICE_ENTRIES.min(n.saturating_sub(prev));
        if min_step == 0 {
            break;
        }
        cut = cut.max(prev + min_step).min(n);
        if cut <= prev {
            break;
        }
        cuts.push(cut);
    }
    if cuts.last().copied() != Some(n) {
        cuts.push(n);
    }
    cuts.windows(2)
        .map(|pair| pair[0]..pair[1])
        .filter(|range| !range.is_empty())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn collect_blob_lookups_for_index_slice<S, P>(
    index: &Index,
    plan: &BlobScanPlan,
    slice: std::ops::Range<usize>,
    symlink_probe: &S,
    dir_abs: &DirAbsFn,
    file_abs: &DirAbsFn,
    literal_paths: bool,
    sparse: BlobScanSparseMode,
    local: &mut Vec<(usize, BlobDiskLookup)>,
    scanned: &mut usize,
    worker_watch: Option<&Arc<AtomicBool>>,
    cancel_stride: &mut usize,
) -> Result<()>
where
    S: Fn() -> P,
    P: FnMut(&[u8]) -> bool,
{
    if slice.is_empty() {
        return Ok(());
    }
    let boundary = StatBoundaryPoll::Worker(worker_watch);
    let mut has_symlink_ancestor = symlink_probe();
    let mut dir_idx = first_dir_index_at_or_after(plan, slice.start);
    while dir_idx < plan.dirs.len() {
        boundary.poll_directory()?;
        let Some(dir_range) = plan.dir_ranges.get(dir_idx) else {
            dir_idx += 1;
            continue;
        };
        if dir_range.start >= slice.end {
            break;
        }
        let overlap_start = dir_range.start.max(slice.start);
        let overlap_end = dir_range.end.min(slice.end);
        if overlap_start < overlap_end {
            let dir = &plan.dirs[dir_idx];
            let entry_indices = &plan.indices[overlap_start..overlap_end];
            let under_symlink =
                dir_has_symlink_ancestor(index, entry_indices, &mut has_symlink_ancestor);
            collect_blob_lookups_in_directory(
                index,
                dir,
                entry_indices,
                dir_abs,
                file_abs,
                literal_paths,
                under_symlink,
                local,
                sparse,
                scanned,
                boundary,
                cancel_stride,
            )?;
        }
        dir_idx += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn preload_blob_lookups_parallel<S, P>(
    index: &Index,
    plan: &BlobScanPlan,
    threads: usize,
    symlink_probe: &S,
    dir_abs: Arc<DirAbsFn>,
    file_abs: Arc<DirAbsFn>,
    literal_paths: bool,
    sparse: BlobScanSparseMode,
    cancel: StatScanCancel,
) -> Result<Vec<Vec<(usize, BlobDiskLookup)>>>
where
    S: Fn() -> P + Sync,
    P: FnMut(&[u8]) -> bool,
{
    let blob_count = plan.len();
    if plan.dirs.is_empty() {
        return Ok(Vec::new());
    }
    let slices = partition_blob_lookup_slices(plan, threads);
    let worker_count = slices.len();
    let mut batches = Vec::with_capacity(worker_count);
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let worker_watch = cancel.watch.clone();
        for slice in slices {
            let dir_abs = Arc::clone(&dir_abs);
            let file_abs = Arc::clone(&file_abs);
            let worker_watch = worker_watch.clone();
            let done_tx = done_tx.clone();
            scope.spawn(move || {
                let result = (|| {
                    let mut local = Vec::new();
                    let mut scanned = 0usize;
                    let mut cancel_stride = 0usize;
                    collect_blob_lookups_for_index_slice(
                        index,
                        plan,
                        slice,
                        symlink_probe,
                        dir_abs.as_ref(),
                        file_abs.as_ref(),
                        literal_paths,
                        sparse,
                        &mut local,
                        &mut scanned,
                        worker_watch.as_ref(),
                        &mut cancel_stride,
                    )?;
                    Ok((local, scanned))
                })();
                let _ = done_tx.send(result);
            });
        }
        drop(done_tx);

        let mut scanned_total = 0usize;
        while batches.len() < worker_count {
            match done_rx.recv_timeout(Duration::from_millis(5)) {
                Ok(Ok((local, scanned))) => {
                    scanned_total += scanned;
                    batches.push(local);
                }
                Ok(Err(e)) => return Err(e),
                Err(RecvTimeoutError::Timeout) => cancel.poll()?,
                Err(RecvTimeoutError::Disconnected) => {
                    if batches.len() < worker_count {
                        return Err(Error::Message(
                            "parallel blob stat worker exited before reporting".to_owned(),
                        ));
                    }
                }
            }
        }
        if scanned_total != blob_count {
            return Err(Error::Message(format!(
                "parallel blob stat preload expected {blob_count} scanned entries, got {scanned_total}"
            )));
        }
        Ok(())
    })?;
    Ok(batches)
}

/// Visit grouped blob entries with one `read_dir` per distinct parent directory.
///
/// When `has_symlink_ancestor(rel_path)` is true, the entry is reported as [`BlobDiskLookup::Missing`]
/// without extra I/O (symlink replaced a directory on the path).
pub(crate) fn for_each_blob_by_directory<F, H>(
    index: &Index,
    plan: &BlobScanPlan,
    dir_abs: impl Fn(&[u8]) -> PathBuf,
    file_abs: impl Fn(&[u8]) -> PathBuf,
    mut has_symlink_ancestor: H,
    visit: F,
) -> Result<()>
where
    F: FnMut(usize, BlobDiskLookup) -> Result<()>,
    H: FnMut(&[u8]) -> bool,
{
    let empty_cancel = empty_stat_cancel();
    let (batch, scanned) = collect_blob_lookups_serial(
        index,
        plan,
        &mut has_symlink_ancestor,
        &dir_abs,
        &file_abs,
        false,
        BlobScanSparseMode::Dense,
        &empty_cancel,
    )?;
    if scanned != plan.len() {
        return Err(Error::Message(format!(
            "blob scan expected {} entries, scanned {scanned}",
            plan.len()
        )));
    }
    visit_blob_lookup_batches(vec![batch], visit)
}

fn empty_stat_cancel() -> StatScanCancel {
    StatScanCancel {
        watch: None,
        owner_poll: None,
    }
}

/// Collect disk lookups for every entry in `plan`, optionally using parallel preload.
///
/// Returns one batch per parallel worker (or a single batch when serial). Callers should
/// consume batches with [`visit_blob_lookup_batches`] so index mutations are not blocked
/// by an active immutable borrow on [`Index`].
///
/// `symlink_probe()` runs once per scan worker and returns that worker's
/// `has_symlink_ancestor(rel_path)` check, which is then asked once per plan directory in
/// index order. Workers own their probe, so it may keep state without locking; pass
/// [`no_symlink_probe`] to skip the check.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_blob_lookup_batches<S, P>(
    index: &Index,
    plan: &BlobScanPlan,
    scan_options: WorktreeBlobScanOptions,
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    dir_abs: impl Fn(&[u8]) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&[u8]) -> PathBuf + Send + Sync + 'static,
    symlink_probe: S,
    sparse: BlobScanSparseMode,
) -> Result<Vec<Vec<(usize, BlobDiskLookup)>>>
where
    S: Fn() -> P + Sync,
    P: FnMut(&[u8]) -> bool,
{
    collect_blob_lookup_batches_with_literal_paths(
        index,
        plan,
        scan_options,
        thread_policy,
        config,
        dir_abs,
        file_abs,
        false,
        symlink_probe,
        sparse,
        None,
        None,
    )
}

/// Symlink probe factory for scans that skip the ancestor-symlink check.
pub(crate) fn no_symlink_probe() -> impl FnMut(&[u8]) -> bool {
    |_| false
}

/// Collect lookups with directory-relative stats when `literal_paths` is true.
///
/// Only enable this when `file_abs` joins the raw index path without case or Unicode
/// resolution. On 64-bit Linux multi-file directories then share one parent handle;
/// other directories and platforms use `file_abs` as usual. `symlink_probe` is as in
/// [`collect_blob_lookup_batches`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_blob_lookup_batches_with_literal_paths<S, P>(
    index: &Index,
    plan: &BlobScanPlan,
    scan_options: WorktreeBlobScanOptions,
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    dir_abs: impl Fn(&[u8]) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&[u8]) -> PathBuf + Send + Sync + 'static,
    literal_paths: bool,
    symlink_probe: S,
    sparse: BlobScanSparseMode,
    cancel_watch: Option<Arc<AtomicBool>>,
    cancel_poll: Option<Arc<crate::progress::StackCancelPoll>>,
) -> Result<Vec<Vec<(usize, BlobDiskLookup)>>>
where
    S: Fn() -> P + Sync,
    P: FnMut(&[u8]) -> bool,
{
    let blob_count = plan.len();
    let cancel = StatScanCancel {
        watch: cancel_watch,
        owner_poll: cancel_poll,
    };
    // Keep small scans on the existing metadata path: setting up descriptor-relative
    // probes pays off on the larger indexes that also justify stat preload.
    let literal_paths = literal_paths && blob_count >= PARALLEL_STAT_MIN_ENTRIES;
    let threads = stat_scan_thread_count(thread_policy, config, scan_options, blob_count);
    let dir_abs_arc: Arc<DirAbsFn> = Arc::new(dir_abs);
    let file_abs_arc: Arc<DirAbsFn> = Arc::new(file_abs);
    let batches = if threads > 1 {
        preload_blob_lookups_parallel(
            index,
            plan,
            threads,
            &symlink_probe,
            Arc::clone(&dir_abs_arc),
            Arc::clone(&file_abs_arc),
            literal_paths,
            sparse,
            cancel,
        )?
    } else {
        let (batch, scanned) = collect_blob_lookups_serial(
            index,
            plan,
            &mut symlink_probe(),
            dir_abs_arc.as_ref(),
            file_abs_arc.as_ref(),
            literal_paths,
            sparse,
            &cancel,
        )?;
        if scanned != blob_count {
            return Err(Error::Message(format!(
                "blob scan expected {blob_count} scanned entries, got {scanned}"
            )));
        }
        vec![batch]
    };
    Ok(batches)
}

/// Like [`for_each_blob_by_directory`], optionally preloading disk lookups in parallel.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn for_each_blob_by_directory_parallel<F, S, P>(
    index: &Index,
    plan: &BlobScanPlan,
    scan_options: WorktreeBlobScanOptions,
    thread_policy: &crate::parallel::ThreadPolicy,
    config: &crate::config::ConfigSet,
    dir_abs: impl Fn(&[u8]) -> PathBuf + Send + Sync + 'static,
    file_abs: impl Fn(&[u8]) -> PathBuf + Send + Sync + 'static,
    symlink_probe: S,
    visit: F,
) -> Result<()>
where
    F: FnMut(usize, BlobDiskLookup) -> Result<()>,
    S: Fn() -> P + Sync,
    P: FnMut(&[u8]) -> bool,
{
    let batches = collect_blob_lookup_batches(
        index,
        plan,
        scan_options,
        thread_policy,
        config,
        dir_abs,
        file_abs,
        symlink_probe,
        BlobScanSparseMode::Dense,
    )?;
    visit_blob_lookup_batches(batches, visit)
}

fn dir_entry_name_matches(index_file_name: &[u8], entry: &fs::DirEntry) -> bool {
    let disk = entry.file_name();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let disk = disk.as_os_str().as_bytes();
        if disk == index_file_name {
            return true;
        }
    }
    #[cfg(not(unix))]
    {
        let disk = disk.to_string_lossy();
        if disk.as_bytes() == index_file_name {
            return true;
        }
    }
    #[cfg(windows)]
    {
        let disk = disk.to_string_lossy();
        if let Ok(index) = std::str::from_utf8(index_file_name) {
            return disk.eq_ignore_ascii_case(index);
        }
    }
    false
}

/// Resolve a directory listing entry for `index_file_name`, including case-insensitive matches.
#[cfg(windows)]
fn matching_dir_entry<'a>(
    on_disk: &'a DirectoryMap,
    index_file_name: &[u8],
) -> Option<&'a fs::DirEntry> {
    if let Some(entry) = on_disk.get(index_file_name) {
        return Some(entry);
    }
    on_disk
        .values()
        .find(|entry| dir_entry_name_matches(index_file_name, entry))
}

fn symlink_metadata_blob_lookup(target_abs: &Path) -> BlobDiskLookup {
    match symlink_metadata_path(target_abs) {
        Ok(meta) => BlobDiskLookup::Present(BlobDiskStat::from_metadata(&meta)),
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            BlobDiskLookup::Missing
        }
        Err(e) => BlobDiskLookup::Io(e),
    }
}

/// Prefer `DirEntry::metadata` from the directory scan (no extra handle open on Windows);
/// fall back to path `symlink_metadata` when the name was not listed or dir-entry stat fails
/// (#1518, #1622). Index disagreements with the scan stat are corrected later by
/// [`align_lookup_stat_with_index_path_windows`].
#[cfg(windows)]
fn blob_lookup_from_dir_entry_or_path(
    dir_entry: Option<&fs::DirEntry>,
    target_abs: impl FnOnce() -> PathBuf,
) -> BlobDiskLookup {
    if let Some(entry) = dir_entry {
        match dir_entry_metadata(entry) {
            Ok(m) => BlobDiskLookup::Present(BlobDiskStat::from_metadata(&m)),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::NotADirectory =>
            {
                symlink_metadata_blob_lookup(&target_abs())
            }
            Err(e) => BlobDiskLookup::Io(e),
        }
    } else {
        symlink_metadata_blob_lookup(&target_abs())
    }
}

/// `target_abs` builds the indexed absolute path for a fallback stat when the name is absent
/// from the directory listing.
fn lookup_blob_in_directory(
    on_disk: &DirectoryMap,
    index_file_name: &[u8],
    target_abs: impl FnOnce() -> PathBuf,
) -> BlobDiskLookup {
    #[cfg(windows)]
    if let Some(entry) = matching_dir_entry(on_disk, index_file_name) {
        return blob_lookup_from_dir_entry_or_path(Some(entry), target_abs);
    }
    if let Some(entry) = on_disk.get(index_file_name) {
        return match dir_entry_metadata(entry) {
            Ok(m) => BlobDiskLookup::Present(BlobDiskStat::from_metadata(&m)),
            Err(e) => BlobDiskLookup::Io(e),
        };
    }
    for entry in on_disk.values() {
        if dir_entry_name_matches(index_file_name, entry) {
            return match dir_entry_metadata(entry) {
                Ok(m) => BlobDiskLookup::Present(BlobDiskStat::from_metadata(&m)),
                Err(e) => BlobDiskLookup::Io(e),
            };
        }
    }
    symlink_metadata_blob_lookup(&target_abs())
}

/// One directory listing keyed by raw file name (`.git` excluded).
type DirectoryMap = hashbrown::HashMap<Vec<u8>, fs::DirEntry>;

enum DirectoryReadOutcome {
    Listed(DirectoryMap),
    Missing,
    Inaccessible(io::Error),
}

/// `expected` sizes the map up front (the tracked entries in this directory).
fn read_directory_map(abs_dir: &Path, expected: usize) -> Result<DirectoryReadOutcome> {
    if probe_counts(abs_dir) {
        WORKTREE_READ_DIR_PROBE.fetch_add(1, Ordering::Relaxed);
    }
    match fs::read_dir(abs_dir) {
        Ok(rd) => {
            let mut on_disk = DirectoryMap::with_capacity(expected);
            for item in rd {
                let Ok(entry) = item else { continue };
                let name = dir_entry_key(&entry);
                if name.as_slice() == b".git" {
                    continue;
                }
                on_disk.insert(name, entry);
            }
            Ok(DirectoryReadOutcome::Listed(on_disk))
        }
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            Ok(DirectoryReadOutcome::Missing)
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            Ok(DirectoryReadOutcome::Inaccessible(e))
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

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn directory_relative_stats_match_metadata_fields_and_file_types() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::os::unix::net::UnixListener;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let oid = ObjectId::null(crate::objects::HashAlgo::Sha1);
        let mut index = Index::new();
        for name in [
            "file",
            "executable",
            "directory",
            "link",
            "socket",
            "missing",
        ] {
            let path = root.join(name);
            fs::write(&path, b"file\n").unwrap();
            index.add_or_replace(entry_from_stat(&path, name.as_bytes(), oid, 0o100644).unwrap());
        }
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("file"))
            .unwrap()
            .set_len(1_u64 << 32)
            .unwrap();
        filetime::set_file_mtime(
            root.join("file"),
            filetime::FileTime::from_unix_time(-1, 123_456),
        )
        .unwrap();
        fs::set_permissions(root.join("executable"), fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["directory", "link", "socket", "missing"] {
            fs::remove_file(root.join(name)).unwrap();
        }
        fs::create_dir(root.join("directory")).unwrap();
        symlink("absent", root.join("link")).unwrap();
        let _socket = UnixListener::bind(root.join("socket")).unwrap();
        // Search permission suffices for lstat: the directory need not be readable.
        fs::set_permissions(root, fs::Permissions::from_mode(0o111)).unwrap();
        let handle = open_stat_directory(root).unwrap();
        let indices: Vec<_> = (0..index.entries.len()).collect();
        let mut cancel_stride = 0usize;
        let boundary = StatBoundaryPoll::Worker(None);
        let result = for_each_blob_at_directory(
            &index,
            &indices,
            &handle,
            root,
            boundary,
            &mut cancel_stride,
            &mut |i, rel, lookup| {
                let expected =
                    fs::symlink_metadata(crate::index::worktree_path_from_index_rel(root, rel));
                match (lookup, expected) {
                    (BlobDiskLookup::Present(actual), Ok(metadata)) => {
                        let expected = BlobDiskStat::from_metadata(&metadata);
                        assert_eq!(actual.git_mode, expected.git_mode);
                        assert_eq!(actual.is_dir, expected.is_dir);
                        assert_eq!(
                            actual.fields.index_entry(rel, oid, actual.git_mode),
                            expected.fields.index_entry(rel, oid, expected.git_mode),
                            "entry {i}"
                        );
                    }
                    (BlobDiskLookup::Missing, Err(error)) => {
                        assert_eq!(error.kind(), io::ErrorKind::NotFound)
                    }
                    _ => panic!("relative stat disagrees with path metadata for entry {i}"),
                }
                Ok(())
            },
        );
        fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
    }

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn directory_relative_stats_reject_embedded_nul() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file");
        fs::write(&path, b"file\n").unwrap();
        let oid = ObjectId::null(crate::objects::HashAlgo::Sha1);
        let mut index = Index::new();
        index.add_or_replace(entry_from_stat(&path, b"file\0suffix", oid, 0o100644).unwrap());
        let handle = open_stat_directory(tmp.path()).unwrap();
        let mut cancel_stride = 0usize;
        let boundary = StatBoundaryPoll::Worker(None);
        let error = for_each_blob_at_directory(
            &index,
            &[0],
            &handle,
            tmp.path(),
            boundary,
            &mut cancel_stride,
            &mut |_, _, _| panic!("invalid name must not be truncated and visited"),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::InvalidInput));
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
        assert_eq!(plan.dirs[0], b"d0".to_vec());

        let mut seen = 0;
        let dir_abs = |dir: &[u8]| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                crate::index::worktree_path_from_index_rel(wt, dir)
            }
        };
        let file_abs = |rel: &[u8]| crate::index::worktree_path_from_index_rel(wt, rel);
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
        let dir_abs = |dir: &[u8]| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                crate::index::worktree_path_from_index_rel(wt, dir)
            }
        };
        let file_abs = |rel: &[u8]| crate::index::worktree_path_from_index_rel(wt, rel);
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
        let dir_abs = |dir: &[u8]| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                crate::index::worktree_path_from_index_rel(wt, dir)
            }
        };
        let file_abs = |rel: &[u8]| crate::index::worktree_path_from_index_rel(wt, rel);
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
        #[cfg(windows)]
        {
            assert!(
                worktree_read_dir_probe_count() >= CHAINS,
                "Windows deep leaf dirs should read_dir each parent (got {} read_dirs)",
                worktree_read_dir_probe_count()
            );
            assert!(
                worktree_metadata_probe_count() <= CHAINS + 5,
                "read_dir scan should stat via DirEntry metadata, got {}",
                worktree_metadata_probe_count()
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                worktree_read_dir_probe_count(),
                0,
                "Unix single-file leaf dirs should use direct stat, not read_dir"
            );
            assert!(
                worktree_metadata_probe_count() <= CHAINS + 5,
                "direct stat scan budget exceeded: {}",
                worktree_metadata_probe_count()
            );
        }
    }

    #[test]
    fn parallel_scan_multi_file_directories_follow_platform_stat_strategy() {
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
        let dir_abs = move |dir: &[u8]| {
            if dir.is_empty() {
                wt_dir.clone()
            } else {
                crate::index::worktree_path_from_index_rel(&wt_dir, dir)
            }
        };
        let wt_file = wt.clone();
        let file_abs = move |rel: &[u8]| crate::index::worktree_path_from_index_rel(&wt_file, rel);
        let policy = crate::parallel::ThreadPolicy::default();
        let config = crate::config::ConfigSet::new();
        let mut seen = 0;
        for_each_blob_by_directory_parallel(
            &index,
            &plan,
            WorktreeBlobScanOptions {
                preload_index: true,
                stat_parallel_threads: Some(4),
                ..WorktreeBlobScanOptions::default()
            },
            &policy,
            &config,
            dir_abs,
            file_abs,
            no_symlink_probe,
            |_, lookup| {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, DIRS * FILES_PER_DIR);
        #[cfg(windows)]
        assert!(
            worktree_read_dir_probe_count() >= DIRS,
            "parallel preload must read_dir each multi-file parent (got {} read_dirs)",
            worktree_read_dir_probe_count()
        );
        #[cfg(not(windows))]
        assert_eq!(
            worktree_read_dir_probe_count(),
            0,
            "Unix stats tracked paths directly instead of listing parents"
        );
        let tracked = DIRS * FILES_PER_DIR;
        assert!(
            worktree_metadata_probe_count() <= tracked + DIRS + 8,
            "metadata probes {} exceeded budget for grouped scan",
            worktree_metadata_probe_count()
        );
    }

    #[test]
    fn lookup_blob_present_when_read_dir_lists_file_but_path_probe_misses() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("leaf");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tracked.txt"), b"payload\n").unwrap();
        let DirectoryReadOutcome::Listed(map) = read_directory_map(&dir, 1).unwrap() else {
            panic!("expected readable directory listing");
        };
        let lookup = lookup_blob_in_directory(&map, b"tracked.txt", || {
            dir.join("wrong-parent/tracked.txt")
        });
        assert!(
            matches!(lookup, BlobDiskLookup::Present(_)),
            "read_dir listing must win over a failed path stat (issue #1518 / Bugbot)"
        );
    }

    #[test]
    fn listed_blob_lookup_one_metadata_probe_per_file() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().to_path_buf();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        const FILES: usize = 40;
        let mut index = Index::new();
        for f in 0..FILES {
            let rel = format!("flat/f{f}.txt");
            let oid = write_blob_file(&wt, &rel, format!("{f}\n").as_bytes());
            index.add_or_replace(
                entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
            );
        }
        let plan = group_blob_entries_by_dir(&index, false);
        let _probe = reset_worktree_metadata_probe(&wt);
        let wt_dir = wt.clone();
        let dir_abs = move |dir: &[u8]| {
            if dir.is_empty() {
                wt_dir.clone()
            } else {
                crate::index::worktree_path_from_index_rel(&wt_dir, dir)
            }
        };
        let file_abs = {
            let wt_dir = wt.clone();
            move |rel: &[u8]| crate::index::worktree_path_from_index_rel(&wt_dir, rel)
        };
        let mut seen = 0usize;
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
        assert_eq!(seen, FILES);
        assert!(
            worktree_metadata_probe_count() <= FILES + 4,
            "listed files should stat via read_dir entries, not a path probe each (got {} probes)",
            worktree_metadata_probe_count()
        );
    }

    #[test]
    fn dense_multi_file_dirs_list_each_parent_at_most_once() {
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
        let dir_abs = move |dir: &[u8]| {
            if dir.is_empty() {
                wt_dir.clone()
            } else {
                crate::index::worktree_path_from_index_rel(&wt_dir, dir)
            }
        };
        let wt_file = wt.clone();
        let file_abs = move |rel: &[u8]| crate::index::worktree_path_from_index_rel(&wt_file, rel);
        let policy = crate::parallel::ThreadPolicy::default();
        let config = crate::config::ConfigSet::new();
        let mut seen = 0;
        for_each_blob_by_directory_parallel(
            &index,
            &plan,
            WorktreeBlobScanOptions {
                preload_index: true,
                stat_parallel_threads: Some(4),
                ..WorktreeBlobScanOptions::default()
            },
            &policy,
            &config,
            dir_abs,
            file_abs,
            no_symlink_probe,
            |_, lookup| {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, DIRS * FILES_PER_DIR);
        let expected_read_dirs = if LIST_PARENT_DIRECTORIES { DIRS } else { 0 };
        assert_eq!(
            worktree_read_dir_probe_count(),
            expected_read_dirs,
            "each dense parent directory is listed once on Windows and never elsewhere"
        );
        let tracked = DIRS * FILES_PER_DIR;
        assert!(
            worktree_metadata_probe_count() <= tracked + DIRS + 8,
            "metadata probes {} exceeded budget for grouped scan",
            worktree_metadata_probe_count()
        );
    }

    #[test]
    fn fat_single_directory_splits_parallel_stat_batches() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().to_path_buf();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        const FILES: usize = PARALLEL_STAT_MIN_ENTRIES + 500;
        let mut index = Index::new();
        for i in 0..FILES {
            let rel = format!("data/file_{i:07}.txt");
            let oid = write_blob_file(&wt, &rel, format!("content {i}\n").as_bytes());
            index.add_or_replace(
                entry_from_stat(&wt.join(&rel), rel.as_bytes(), oid, 0o100644).unwrap(),
            );
        }
        let plan = group_blob_entries_by_dir(&index, false);
        assert_eq!(plan.dirs.len(), 1);
        let wt_dir = wt.clone();
        let dir_abs = move |dir: &[u8]| {
            if dir.is_empty() {
                wt_dir.clone()
            } else {
                crate::index::worktree_path_from_index_rel(&wt_dir, dir)
            }
        };
        let wt_file = wt.clone();
        let file_abs = move |rel: &[u8]| crate::index::worktree_path_from_index_rel(&wt_file, rel);
        let policy = crate::parallel::ThreadPolicy::default();
        let config = crate::config::ConfigSet::new();
        let batches = collect_blob_lookup_batches(
            &index,
            &plan,
            WorktreeBlobScanOptions {
                preload_index: true,
                stat_parallel_threads: Some(4),
                ..WorktreeBlobScanOptions::default()
            },
            &policy,
            &config,
            dir_abs,
            file_abs,
            no_symlink_probe,
            BlobScanSparseMode::Dense,
        )
        .unwrap();
        assert!(
            batches.len() > 1,
            "one huge directory should split across workers, got {} batches",
            batches.len()
        );
        let mut seen = std::collections::BTreeSet::new();
        for batch in &batches {
            for (entry_index, lookup) in batch {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                assert!(seen.insert(*entry_index));
            }
        }
        assert_eq!(seen.len(), FILES);
        let _probe = reset_worktree_metadata_probe(&wt);
        let mut rescanned = 0usize;
        for_each_blob_by_directory_parallel(
            &index,
            &plan,
            WorktreeBlobScanOptions {
                preload_index: true,
                stat_parallel_threads: Some(4),
                ..WorktreeBlobScanOptions::default()
            },
            &policy,
            &config,
            {
                let wt_dir = wt.clone();
                move |dir: &[u8]| {
                    if dir.is_empty() {
                        wt_dir.clone()
                    } else {
                        crate::index::worktree_path_from_index_rel(&wt_dir, dir)
                    }
                }
            },
            {
                let wt_file = wt.clone();
                move |rel: &[u8]| crate::index::worktree_path_from_index_rel(&wt_file, rel)
            },
            no_symlink_probe,
            |_, lookup| {
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                rescanned += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(rescanned, FILES);
        assert!(
            worktree_metadata_probe_count() <= FILES + 4,
            "parallel fat-dir scan should stat about once per file, got {}",
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
                ..WorktreeBlobScanOptions::default()
            },
            PARALLEL_STAT_MIN_ENTRIES,
        );
        assert_eq!(threads, 1);
    }

    #[test]
    fn parallel_stat_without_preload_uses_multiple_workers() {
        let threads = stat_scan_thread_count(
            &crate::parallel::ThreadPolicy::default(),
            &crate::config::ConfigSet::new(),
            WorktreeBlobScanOptions {
                preload_index: false,
                parallel_stat_without_preload: true,
                ..WorktreeBlobScanOptions::default()
            },
            PARALLEL_STAT_MIN_ENTRIES + 500,
        );
        assert!(
            threads > 1,
            "hard-reset-style scan should pick parallel workers (got {threads})"
        );
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

    #[cfg(windows)]
    #[test]
    fn align_lookup_keeps_dir_scan_stat_when_path_probe_fails() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let rel = "probe-fail.txt";
        let oid = write_blob_file(wt, rel, b"payload\n");
        let mut index = Index::new();
        index
            .add_or_replace(entry_from_stat(&wt.join(rel), rel.as_bytes(), oid, 0o100644).unwrap());
        let dir_stat = BlobDiskStat::from_metadata(
            &fs::symlink_metadata(wt.join(rel)).expect("worktree metadata"),
        );
        let policy = crate::index::StatMatchPolicy::default();
        let lookup = align_lookup_stat_with_index_path_windows(
            &index,
            0,
            BlobDiskLookup::Present(dir_stat),
            || PathBuf::from("Z:\\no-such-path-for-align-probe\\missing.txt"),
            policy,
        );
        assert!(
            matches!(lookup, BlobDiskLookup::Present(s)),
            "path probe failure must not discard a successful directory scan"
        );
        if let BlobDiskLookup::Present(s) = lookup {
            assert_eq!(s.fields.index_size(), dir_stat.fields.index_size());
        }
    }

    #[cfg(windows)]
    const NSEC_POLICY: crate::index::StatMatchPolicy = crate::index::StatMatchPolicy {
        trust_ctime: true,
        check_stat: true,
        use_nanosec: true,
    };

    #[cfg(windows)]
    #[test]
    fn align_lookup_skips_path_probe_when_scan_fractions_match() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let rel = "clean.txt";
        let oid = write_blob_file(wt, rel, b"payload\n");
        let mut index = Index::new();
        let mut ie = entry_from_stat(&wt.join(rel), rel.as_bytes(), oid, 0o100644).unwrap();
        ie.ctime_sec += 1;
        index.add_or_replace(ie);
        let scan = BlobDiskStat::from_metadata(&fs::symlink_metadata(wt.join(rel)).unwrap());
        let lookup = align_lookup_stat_with_index_path_windows(
            &index,
            0,
            BlobDiskLookup::Present(scan),
            || panic!("whole-second ctime disagreement alone must not probe the path"),
            NSEC_POLICY,
        );
        assert!(matches!(lookup, BlobDiskLookup::Present(_)));
    }

    #[cfg(windows)]
    #[test]
    fn align_lookup_probes_path_when_scan_mtime_fraction_disagrees() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "indexed.txt", b"payload\n");
        write_blob_file(wt, "scanned.txt", b"payload\n");
        let pinned = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_700);
        let later = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_800);
        filetime::set_file_mtime(wt.join("indexed.txt"), pinned).unwrap();
        filetime::set_file_mtime(wt.join("scanned.txt"), later).unwrap();
        let mut index = Index::new();
        index.add_or_replace(
            entry_from_stat(&wt.join("indexed.txt"), b"indexed.txt", oid, 0o100644).unwrap(),
        );
        let policy = crate::index::StatMatchPolicy {
            trust_ctime: false,
            ..NSEC_POLICY
        };
        let scan =
            BlobDiskStat::from_metadata(&fs::symlink_metadata(wt.join("scanned.txt")).unwrap());
        assert!(!crate::diff::stat_matches_blob_stat(
            &index.entries[0],
            &scan,
            policy
        ));

        let lookup = align_lookup_stat_with_index_path_windows(
            &index,
            0,
            BlobDiskLookup::Present(scan),
            || wt.join("indexed.txt"),
            policy,
        );
        let BlobDiskLookup::Present(stat) = lookup else {
            panic!("expected a present lookup");
        };
        assert!(
            crate::diff::stat_matches_blob_stat(&index.entries[0], &stat, policy),
            "a matching path probe must replace the scan stat"
        );
    }

    /// A clean checkout must status clean without opening tracked files, even one another
    /// process holds open with no sharing (issue #2175).
    #[cfg(windows)]
    #[test]
    fn status_after_checkout_is_clean_with_an_exclusively_locked_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let mut index = Index::new();
        for rel in ["a.txt", "dir/b.txt", "locked.txt"] {
            let oid = write_blob_file(wt, rel, format!("{rel}\n").as_bytes());
            index.add_or_replace(
                entry_from_stat(&wt.join(rel), rel.as_bytes(), oid, 0o100644).unwrap(),
            );
        }
        index.sort();
        let index_path = wt.join(".git/index");
        index.write(&index_path).unwrap();
        let (sec, nsec) = crate::index::stage0_mtime_max(&index.entries);
        filetime::set_file_mtime(
            &index_path,
            filetime::FileTime::from_unix_time(i64::from(sec), nsec),
        )
        .unwrap();
        crate::index::advance_index_file_mtime_past_entries(&index, &index_path, None).unwrap();

        let _lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(wt.join("locked.txt"))
            .unwrap();
        assert!(
            fs::read(wt.join("locked.txt")).is_err(),
            "file must be unreadable"
        );

        let odb = Odb::new(&wt.join(".git/objects"));
        let (diffs, _) = crate::diff::diff_index_to_worktree_with_options(
            &odb,
            &mut index,
            wt,
            crate::diff::DiffIndexToWorktreeOptions {
                index_mtime: crate::index::index_file_mtime(&index_path),
                ..crate::diff::DiffIndexToWorktreeOptions::default()
            },
        )
        .expect("stat-only status must not read the locked file");
        assert!(diffs.is_empty(), "{diffs:?}");
    }
}
