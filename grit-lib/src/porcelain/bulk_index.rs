//! Thresholds for campaign-scale index staging (tracked restage and UNTR deferral).

/// Entry-count thresholds for bulk tracked restage behavior.
///
/// Passed explicitly into staging APIs so tests can use smaller limits without process-wide state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

impl IndexStagingThresholds {
    /// Sets [`large_index_restage`](Self::large_index_restage).
    #[must_use]
    pub const fn with_large_index_restage(mut self, large_index_restage: usize) -> Self {
        self.large_index_restage = large_index_restage;
        self
    }

    /// Sets [`parallel_tracked_restage`](Self::parallel_tracked_restage).
    #[must_use]
    pub const fn with_parallel_tracked_restage(mut self, parallel_tracked_restage: usize) -> Self {
        self.parallel_tracked_restage = parallel_tracked_restage;
        self
    }
}
