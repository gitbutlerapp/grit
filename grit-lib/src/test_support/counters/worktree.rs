//! Worktree metadata / `read_dir` syscall probes for O(1) scan regressions.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static WORKTREE_METADATA_PROBE: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support worktree probe counters
static WORKTREE_READ_DIR_PROBE: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support worktree probe counters
static WORKTREE_PATH_RESOLVE_PROBE: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support worktree probe counters
static WORKTREE_PROBE_ACTIVE: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support worktree probe counters
static WORKTREE_PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None); // hygiene: test-support worktree probe counters
static WORKTREE_PROBE_SERIAL: Mutex<()> = Mutex::new(()); // hygiene: test-support worktree probe counters

/// Serializes probing tests; each owns the counters while its scope is alive.
/// Guard that scopes worktree syscall probes to one test and repository root.
pub struct WorktreeProbeScope {
    _serial: MutexGuard<'static, ()>,
}

impl Drop for WorktreeProbeScope {
    fn drop(&mut self) {
        *WORKTREE_PROBE_ROOT
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        WORKTREE_PROBE_ACTIVE.store(0, Ordering::Release);
    }
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

/// Zero the probe counters and count only syscalls on paths under `root`.
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

/// Returns metadata syscall probes recorded since [`reset_worktree_metadata_probe`].
pub fn worktree_metadata_probe_count() -> usize {
    WORKTREE_METADATA_PROBE.load(Ordering::Relaxed)
}

/// Returns `read_dir` probes recorded since [`reset_worktree_metadata_probe`].
pub fn worktree_read_dir_probe_count() -> usize {
    WORKTREE_READ_DIR_PROBE.load(Ordering::Relaxed)
}

/// Returns `core.ignorecase` path resolutions recorded since [`reset_worktree_metadata_probe`].
pub fn worktree_path_resolve_probe_count() -> usize {
    WORKTREE_PATH_RESOLVE_PROBE.load(Ordering::Relaxed)
}

pub(crate) fn record_metadata(path: &Path) {
    if probe_counts(path) {
        WORKTREE_METADATA_PROBE.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_read_dir(path: &Path) {
    if probe_counts(path) {
        WORKTREE_READ_DIR_PROBE.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_path_resolve(path: &Path) {
    if probe_counts(path) {
        WORKTREE_PATH_RESOLVE_PROBE.fetch_add(1, Ordering::Relaxed);
    }
}
