//! Per-thread count of lookups into a stored ref map, so tests can assert that transaction
//! checks cost only the refs they touch rather than the size of the store.

use std::cell::Cell;

thread_local! { // hygiene: test-only stored-ref lookup counter (thread-local)
    static STORED_REF_PROBES: Cell<usize> = const { Cell::new(0) };
}

/// Note one lookup (`get`, `contains_key` or a range probe) into a stored ref map.
pub(crate) fn record_stored_ref_probe() {
    STORED_REF_PROBES.with(|c| c.set(c.get().saturating_add(1)));
}

/// Return the lookups recorded on this thread since the last call, and reset the count.
pub(crate) fn take_stored_ref_probes() -> usize {
    STORED_REF_PROBES.with(|c| c.replace(0))
}
