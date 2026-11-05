//! Process-global pack cache test synchronization.

pub use crate::pack::pack_cache_test_sync::PackCacheTestGuard;

/// Serializes tests that mutate the process-global pack cache (including [`crate::objects::pack::clear_pack_cache`]).
#[must_use]
pub fn pack_cache_guard() -> PackCacheTestGuard {
    crate::pack::pack_cache_test_sync::acquire()
}
