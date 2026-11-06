//! Failure, pause, and probe injection hooks for workspace tests.
#![allow(missing_docs)]
//!
//! Setters drive `pub(crate)` injection sites in production modules. Injection
//! checks compile only with `test` or the `test-support` feature enabled.

use std::path::Path;

pub use crate::history::patch_id_inject::{
    patch_id_compute_call_count as test_patch_id_compute_call_count,
    reset_patch_id_compute_call_count as test_reset_patch_id_compute_call_count,
};
pub use crate::refs::inject::{
    branch_ref_pause_gate, pause_injection_test_scope, ref_store_batch_pause_gate,
    ref_store_batch_pause_test_scope, set_branch_ref_pause_before_commit,
    set_ref_store_batch_pause_after_apply, set_reflog_fail, BranchRefPauseGate,
    PauseInjectionTestScope, RefStoreBatchPauseGate, RefStoreBatchPauseTestScope,
};
pub use crate::reftable::inject::set_reftable_transaction_fail as set_test_inject_reftable_transaction_fail;
pub use crate::repo::inject::{
    set_index_write_fail as set_test_inject_index_write_fail,
    set_index_write_fail_remaining as set_test_inject_index_write_fail_remaining,
    set_rewrite_apply_abort_after_refs as set_test_inject_rewrite_apply_abort_after_refs,
};

/// Build a pause gate for [`set_test_inject_ref_store_batch_pause_after_apply`].
#[must_use]
pub fn new_ref_store_batch_pause_gate() -> RefStoreBatchPauseGate {
    ref_store_batch_pause_gate()
}

/// Alias for [`pause_injection_test_scope`].
#[must_use]
pub fn test_pause_injection_serial_guard() -> PauseInjectionTestScope {
    pause_injection_test_scope()
}

/// Alias for [`set_reflog_fail`].
pub fn set_test_inject_reflog_fail(failed_ref: Option<&str>) {
    set_reflog_fail(failed_ref);
}

/// Alias for [`set_branch_ref_pause_before_commit`].
pub fn set_test_inject_branch_ref_pause_before_commit(gate: Option<BranchRefPauseGate>) {
    set_branch_ref_pause_before_commit(gate);
}

/// Alias for [`set_ref_store_batch_pause_after_apply`].
pub fn set_test_inject_ref_store_batch_pause_after_apply(
    apply_index: usize,
    git_dir: &Path,
    gate: Option<RefStoreBatchPauseGate>,
) {
    set_ref_store_batch_pause_after_apply(apply_index, git_dir, gate);
}
