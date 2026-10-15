//! Thresholds for campaign-scale index staging (tracked restage and UNTR deferral).

use std::sync::atomic::{AtomicUsize, Ordering};

/// Parallel blob reread and deferred UNTR invalidation apply at this index entry count and above.
const DEFAULT_LARGE_INDEX_RESTAGE_THRESHOLD: usize = 80_000;

static TEST_THRESHOLD_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Override the bulk threshold for the current process (tests only).
#[cfg(test)]
pub(crate) fn set_large_index_restage_threshold_for_test(threshold: usize) {
    TEST_THRESHOLD_OVERRIDE.store(threshold, Ordering::SeqCst);
}

/// Clear a test override set by [`set_large_index_restage_threshold_for_test`].
#[cfg(test)]
pub(crate) fn clear_large_index_restage_threshold_for_test() {
    TEST_THRESHOLD_OVERRIDE.store(0, Ordering::SeqCst);
}

/// Process-wide threshold override for integration tests (reset with [`clear_test_threshold_override`]).
#[doc(hidden)]
pub fn set_test_threshold_override(threshold: usize) {
    TEST_THRESHOLD_OVERRIDE.store(threshold, Ordering::SeqCst);
}

/// Clear [`set_test_threshold_override`].
#[doc(hidden)]
pub fn clear_test_threshold_override() {
    TEST_THRESHOLD_OVERRIDE.store(0, Ordering::SeqCst);
}

/// Entry count at which bulk restage optimizations activate.
pub(crate) fn large_index_restage_threshold() -> usize {
    let override_n = TEST_THRESHOLD_OVERRIDE.load(Ordering::SeqCst);
    if override_n > 0 {
        return override_n;
    }
    DEFAULT_LARGE_INDEX_RESTAGE_THRESHOLD
}
