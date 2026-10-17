//! Hyperfine `--prepare` hook: restore the maintenance benchmark worktree.

use std::path::PathBuf;

pub use crate::maintenance_suite::run_prepare_maintenance;

const BARE_MARKER: &str = "maintenance-bare.path";

/// Path written by [`crate::maintenance_suite::run_maintenance_suite`] for prepare resets.
pub fn maintenance_bare_marker_path() -> PathBuf {
    crate::fixture::scratch_dir().join(BARE_MARKER)
}
