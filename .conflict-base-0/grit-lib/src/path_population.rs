//! Path checks when flattening trees into the index or writing the worktree.

use crate::error::{Error, Result};
use crate::path_validate::{
    validate_component, validate_path, EntryMode, PathValidationKind, ProtectOptions,
};

/// Validate one tree entry name while walking toward index entries.
///
/// `prefix` is the repository-relative path to the parent directory (`""` at the root).
/// The full path string is built only when validation fails.
pub fn validate_tree_walk_entry(
    prefix: &[u8],
    entry_name: &[u8],
    mode: u32,
    opts: ProtectOptions,
) -> Result<()> {
    validate_component(
        entry_name,
        EntryMode::new(mode),
        opts,
        PathValidationKind::Worktree,
    )
    .map_err(|reason| {
        let display = tree_walk_display_path(prefix, entry_name);
        Error::invalid_path_component(display, reason)
    })
}

fn tree_walk_display_path(prefix: &[u8], entry_name: &[u8]) -> String {
    let name = String::from_utf8_lossy(entry_name);
    if prefix.is_empty() {
        name.into_owned()
    } else {
        format!("{}/{}", String::from_utf8_lossy(prefix), name)
    }
}

/// Validate a full repository-relative path before a worktree write.
pub fn validate_worktree_path(path: &[u8], mode: u32, opts: ProtectOptions) -> Result<()> {
    validate_path(
        path,
        EntryMode::new(mode),
        opts,
        PathValidationKind::Worktree,
    )
    .map_err(|reason| {
        Error::invalid_path_component(String::from_utf8_lossy(path).into_owned(), reason)
    })
}
