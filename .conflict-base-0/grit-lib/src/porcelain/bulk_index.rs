//! Thresholds for campaign-scale index staging (tracked restage and UNTR deferral).

/// Entry-count thresholds for bulk tracked restage behavior.
///
/// Passed explicitly into staging APIs so tests can use smaller limits without process-wide state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStagingThresholds {
    /// Deferred UNTR invalidation and cache-tree drop at or above this stage-0 entry count.
    pub large_index_restage: usize,
    /// Parallel tracked blob reread batches at or above this entry count.
    pub parallel_tracked_restage: usize,
}

impl Default for IndexStagingThresholds {
    fn default() -> Self {
        Self {
            large_index_restage: 4_096,
            parallel_tracked_restage: 4_096,
        }
    }
}
