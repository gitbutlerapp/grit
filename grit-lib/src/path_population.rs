//! Path checks when flattening trees into the index or writing the worktree.

use crate::error::{Error, Result};
use crate::path_validate::{validate_component, validate_path, EntryMode, ProtectOptions};

/// Validate one tree entry name while walking toward index entries.
///
/// `display_path` is the repository-relative path including this component (used in errors).
pub fn validate_tree_walk_entry(
    display_path: &str,
    entry_name: &[u8],
    mode: u32,
    opts: ProtectOptions,
) -> Result<()> {
    validate_component(entry_name, EntryMode::new(mode), opts)
        .map_err(|reason| Error::invalid_path_component(display_path, reason))
}

/// Validate a full repository-relative path before a worktree write.
pub fn validate_worktree_path(path: &[u8], mode: u32, opts: ProtectOptions) -> Result<()> {
    validate_path(path, EntryMode::new(mode), opts).map_err(|reason| {
        Error::invalid_path_component(String::from_utf8_lossy(path).into_owned(), reason)
    })
}
