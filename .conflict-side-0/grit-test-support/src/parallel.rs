//! Helpers for forcing parallel code paths in library integration tests.

use grit_lib::parallel::Parallelism;
use grit_lib::parallel::{ParallelOp, ParallelOptions, ThreadPolicy};

/// Build a [`ThreadPolicy`] that forces four workers and disables batch thresholds.
#[must_use]
pub fn forced_parallel_policy() -> ThreadPolicy {
    ThreadPolicy::with_threads_override(Some(4))
        .min_items_override(Some(0))
        .min_total_bytes_override(Some(0))
}

/// Resolved options for `op` with [`forced_parallel_policy`].
#[must_use]
pub fn forced_parallel_options(op: ParallelOp) -> ParallelOptions {
    forced_parallel_policy().resolve(op, &grit_lib::config::ConfigSet::new())
}

/// [`Parallelism`] with four workers (clamped to available CPUs).
#[must_use]
pub fn forced_parallelism() -> Parallelism {
    forced_parallel_options(ParallelOp::IndexRefresh).parallelism()
}
