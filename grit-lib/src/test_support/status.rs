//! Status computation hooks for deterministic race tests.

use std::sync::{Arc, Mutex};

static BEFORE_OPPORTUNISTIC_INDEX_WRITE: Mutex<Option<Arc<dyn Fn() + Send + Sync>>> =
    // hygiene: test-support status race hook
    Mutex::new(None);

/// Run `hook` immediately before an opportunistic index write in [`crate::porcelain::status::status`].
///
/// Pass `None` to clear. Only one hook is active process-wide; tests should not run in parallel
/// when a hook is set.
pub fn set_before_opportunistic_index_write_hook(hook: Option<Arc<dyn Fn() + Send + Sync>>) {
    *BEFORE_OPPORTUNISTIC_INDEX_WRITE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = hook;
}

pub(crate) fn invoke_before_opportunistic_index_write() {
    if let Some(hook) = BEFORE_OPPORTUNISTIC_INDEX_WRITE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        hook();
    }
}
