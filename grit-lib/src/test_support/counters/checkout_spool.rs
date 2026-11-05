//! Checkout journal spool-delete probes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static CHECKOUT_SPOOL_PROBE: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support checkout spool probe counters
static CHECKOUT_SPOOL_PROBE_ACTIVE: AtomicUsize = AtomicUsize::new(0); // hygiene: test-support checkout spool probe counters
static CHECKOUT_SPOOL_PROBE_SERIAL: Mutex<bool> = Mutex::new(false); // hygiene: test-support checkout spool probe counters

/// Keeps checkout spool probing armed for one test; disarms on drop.
/// Guard that arms checkout spool-delete counting for one test.
pub struct CheckoutSpoolProbeScope {
    _serial: MutexGuard<'static, bool>,
}

impl Drop for CheckoutSpoolProbeScope {
    fn drop(&mut self) {
        *self._serial = false;
        CHECKOUT_SPOOL_PROBE_ACTIVE.store(0, Ordering::Release);
    }
}

/// Reset and return a guard that counts checkout delete spools until dropped.
pub fn reset_checkout_spool_probe() -> CheckoutSpoolProbeScope {
    let mut serial = CHECKOUT_SPOOL_PROBE_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert!(
        !*serial,
        "checkout spool probe already armed in another test"
    );
    *serial = true;
    CHECKOUT_SPOOL_PROBE.store(0, Ordering::Relaxed);
    CHECKOUT_SPOOL_PROBE_ACTIVE.store(1, Ordering::Release);
    CheckoutSpoolProbeScope { _serial: serial }
}

/// Spool deletes recorded since [`reset_checkout_spool_probe`].
pub fn checkout_spool_probe_count() -> usize {
    CHECKOUT_SPOOL_PROBE.load(Ordering::Relaxed)
}

pub(crate) fn record_spool_delete() {
    if CHECKOUT_SPOOL_PROBE_ACTIVE.load(Ordering::Acquire) != 0 {
        CHECKOUT_SPOOL_PROBE.fetch_add(1, Ordering::Relaxed);
    }
}
