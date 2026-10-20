//! Helpers for forcing parallel code paths in library integration tests.

use std::sync::{Mutex, MutexGuard};

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

static HEAVY_DISK_SERIAL: Mutex<()> = Mutex::new(());

/// Serialize tests that materialize tens of thousands of worktree files.
///
/// Parallel `cargo test` runs otherwise peak above runner disk limits when
/// campaign-scale UNTR and large Git index fixtures run together.
pub fn heavy_disk_serial_guard() -> MutexGuard<'static, ()> {
    HEAVY_DISK_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
