//! Hot-path regression counters for workspace tests (compiled out without `test` / `test-support`).

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod checkout_spool;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod diff_blob_reads;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod has_changes;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod worktree;

#[cfg(any(test, feature = "test-support"))]
pub use crate::worktree_scan::PARALLEL_STAT_MIN_ENTRIES;
#[cfg(any(test, feature = "test-support"))]
pub use checkout_spool::{
    checkout_spool_probe_count, reset_checkout_spool_probe, CheckoutSpoolProbeScope,
};
#[cfg(any(test, feature = "test-support"))]
pub use diff_blob_reads::{track_blob_reads, BlobReadCounterScope};
#[cfg(any(test, feature = "test-support"))]
pub use has_changes::{
    has_changes_call_count, reset_has_changes_call_count,
    reset_worktree_has_changes_entries_scanned, worktree_has_changes_entries_scanned,
};
#[cfg(any(test, feature = "test-support"))]
pub use worktree::{
    reset_worktree_metadata_probe, worktree_metadata_probe_count,
    worktree_path_resolve_probe_count, worktree_read_dir_probe_count, WorktreeProbeScope,
};
