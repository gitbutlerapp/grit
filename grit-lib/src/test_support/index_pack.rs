//! Index-pack ingest thread recording for integration tests.

/// Enable recording of resolved index-pack worker counts.
pub fn set_ingest_thread_recording(enabled: bool) {
    crate::index_pack::set_ingest_thread_recording_for_tests(enabled);
}

/// Last resolved worker count from pack ingest while recording is enabled.
#[must_use]
pub fn last_resolved_ingest_threads() -> Option<usize> {
    crate::index_pack::last_resolved_ingest_threads_for_tests()
}
