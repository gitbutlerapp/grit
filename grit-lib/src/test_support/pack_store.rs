//! Pack-store creation counters for integration tests.

use std::sync::atomic::{AtomicUsize, Ordering};

static LEGACY_STORES_CREATED: AtomicUsize = AtomicUsize::new(0);
static IMPLICIT_STORES_CREATED: AtomicUsize = AtomicUsize::new(0);

/// Reset pack-store creation counters.
pub fn reset() {
    LEGACY_STORES_CREATED.store(0, Ordering::Release);
    IMPLICIT_STORES_CREATED.store(0, Ordering::Release);
}

/// Implicit thread-local pack stores created since the last [`reset`].
#[must_use]
pub fn implicit_stores_created() -> usize {
    IMPLICIT_STORES_CREATED.load(Ordering::Acquire)
}

/// Legacy per-directory pack stores created since the last [`reset`].
#[must_use]
pub fn legacy_stores_created() -> usize {
    LEGACY_STORES_CREATED.load(Ordering::Acquire)
}

pub(crate) fn record_legacy_store_created() {
    LEGACY_STORES_CREATED.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_implicit_store_created() {
    IMPLICIT_STORES_CREATED.fetch_add(1, Ordering::Relaxed);
}
