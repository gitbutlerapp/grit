//! [`Diff::has_changes`](crate::diff::Diff::has_changes) scan probes.

use std::sync::atomic::{AtomicUsize, Ordering};

static WORKTREE_HAS_CHANGES_ENTRIES_SCANNED: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support has_changes probe counters
static HAS_CHANGES_CALL_COUNT: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support has_changes probe counters

/// Entries scanned in the last worktree `has_changes` walk (since last reset).
pub fn worktree_has_changes_entries_scanned() -> usize {
    WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.load(Ordering::SeqCst)
}

/// Clear the worktree entry scan counter before an operation under test.
pub fn reset_worktree_has_changes_entries_scanned() {
    WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.store(0, Ordering::SeqCst);
}

/// How many times [`crate::diff::Diff::has_changes`] ran since the last reset.
pub fn has_changes_call_count() -> usize {
    HAS_CHANGES_CALL_COUNT.load(Ordering::SeqCst)
}

/// Clear the [`crate::diff::Diff::has_changes`] call counter.
pub fn reset_has_changes_call_count() {
    HAS_CHANGES_CALL_COUNT.store(0, Ordering::SeqCst);
}

pub(crate) fn reset_entries_scanned() {
    WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.store(0, Ordering::SeqCst);
}

pub(crate) fn record_entry_scanned() {
    WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn record_has_changes_call() {
    HAS_CHANGES_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
}
