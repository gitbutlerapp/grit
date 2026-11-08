//! Linked-worktree internals for integration tests (`full-tests` / `test-support` only).

use std::path::Path;

use crate::error::Result;

/// Copies filtered `config.worktree` settings into a linked worktree admin dir (Git-compatible).
pub fn copy_filtered_worktree_config(source_git_dir: &Path, admin_dir: &Path) -> Result<()> {
    crate::worktree::copy_filtered_worktree_config(source_git_dir, admin_dir)
}
