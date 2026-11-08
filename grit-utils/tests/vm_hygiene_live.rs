//! Host-level checks for page-cache dropping (not run in parallel with other tests).

#![cfg(target_os = "linux")]

use grit_utils::vm_hygiene::{drop_linux_page_caches, DROP_CACHES_ENV};
use std::sync::{Mutex, MutexGuard};

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Requires passwordless `sudo` or write access to `/proc/sys/vm/drop_caches` (factory VM).
#[test]
fn drop_linux_page_caches_succeeds_on_factory_host() {
    let _guard = env_lock();
    std::env::set_var(DROP_CACHES_ENV, "1");
    let result = drop_linux_page_caches();
    std::env::remove_var(DROP_CACHES_ENV);
    result.expect("live drop_caches on factory VM");
}
