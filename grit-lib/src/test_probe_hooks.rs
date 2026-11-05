//! Hot-path call sites for test counters (no-op unless `test` or `test-support`).

pub(crate) mod hooks {
    use std::path::Path;

    #[inline(always)]
    pub(crate) fn worktree_metadata(path: &Path) {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::worktree::record_metadata(path);
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = path;
    }

    #[inline(always)]
    pub(crate) fn worktree_read_dir(path: &Path) {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::worktree::record_read_dir(path);
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = path;
    }

    #[inline(always)]
    pub(crate) fn worktree_path_resolve(path: &Path) {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::worktree::record_path_resolve(path);
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = path;
    }

    #[inline(always)]
    pub(crate) fn checkout_spool_delete() {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::checkout_spool::record_spool_delete();
    }

    #[inline(always)]
    pub(crate) fn has_changes_call() {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::has_changes::record_has_changes_call();
    }

    #[inline(always)]
    pub(crate) fn reset_worktree_has_changes_scan() {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::has_changes::reset_entries_scanned();
    }

    #[inline(always)]
    pub(crate) fn worktree_has_changes_entry_scanned() {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::has_changes::record_entry_scanned();
    }

    #[inline(always)]
    pub(crate) fn diff_blob_read() {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::counters::diff_blob_reads::note_blob_read();
    }

    #[inline(always)]
    pub(crate) fn before_opportunistic_index_write() {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_support::status::invoke_before_opportunistic_index_write();
    }
}
