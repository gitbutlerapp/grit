//! Thresholds for campaign-scale index staging (tracked restage and UNTR deferral).

use std::sync::OnceLock;

/// Parallel blob reread and deferred UNTR invalidation apply at this index entry count and above.
const DEFAULT_LARGE_INDEX_RESTAGE_THRESHOLD: usize = 80_000;

/// Entry count at which bulk restage optimizations activate.
///
/// Tests may lower this with the `GRIT_TEST_BULK_THRESHOLD` environment variable.
pub(crate) fn large_index_restage_threshold() -> usize {
    static THRESHOLD: OnceLock<usize> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("GRIT_TEST_BULK_THRESHOLD")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_LARGE_INDEX_RESTAGE_THRESHOLD)
    })
}
