//! Map filesystem failures on repository-relative paths to typed [`Error::WorktreePathIo`].
//!
//! Callers pass the index-relative path and what they were doing so Windows sharing
//! violations and Unix `EBUSY` produce a stable, actionable message without copying Git.

use std::io;

use crate::error::{Error, Result};

/// Worktree filesystem operation that failed on a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorktreePathOperation {
    /// Reading file bytes or a symlink target.
    Read,
    /// Replacing an existing file (checkout, hard reset, apply).
    Replace,
    /// Removing a file or symlink before rewrite.
    Remove,
    /// Creating a new path (file or symlink).
    Create,
    /// `stat` / metadata probe without opening for read.
    Metadata,
}

impl WorktreePathOperation {
    fn verb(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Replace => "replace",
            Self::Remove => "remove",
            Self::Create => "create",
            Self::Metadata => "inspect",
        }
    }
}

/// Whether `err` usually means another process holds the file open.
#[must_use]
pub fn is_file_in_use(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    if let Some(raw) = err.raw_os_error() {
        #[cfg(windows)]
        if raw == 32 {
            return true;
        }
        if raw == libc::EBUSY {
            return true;
        }
        #[cfg(windows)]
        if raw == 33 {
            // ERROR_LOCK_VIOLATION
            return true;
        }
    }
    false
}

/// Human-readable summary for [`Error::WorktreePathIo`].
#[must_use]
pub fn format_worktree_path_io_message(
    path: &str,
    operation: WorktreePathOperation,
    source: &io::Error,
) -> String {
    if is_file_in_use(source) {
        format!(
            "cannot {} '{path}': file is in use by another process (close it and retry)",
            operation.verb()
        )
    } else {
        format!("cannot {} '{path}': {source}", operation.verb())
    }
}

/// Wrap a raw I/O failure with path and operation context.
#[must_use]
pub fn map_worktree_io(
    path: impl Into<String>,
    operation: WorktreePathOperation,
    source: io::Error,
) -> Error {
    Error::WorktreePathIo {
        path: path.into(),
        operation,
        source,
    }
}

/// Attach path context to [`std::io::Result`].
pub fn io_with_path<T>(
    path: impl Into<String>,
    operation: WorktreePathOperation,
    result: io::Result<T>,
) -> Result<T> {
    result.map_err(|source| map_worktree_io(path, operation, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn windows_sharing_violation_maps_to_in_use_message() {
        let err = io::Error::from_raw_os_error(32);
        let msg = format_worktree_path_io_message("b.txt", WorktreePathOperation::Replace, &err);
        assert!(msg.contains("b.txt"));
        assert!(msg.contains("in use by another process"));
        assert!(!msg.contains("os error 32"));
    }

    #[test]
    fn ebusy_maps_to_in_use_message() {
        let err = io::Error::from_raw_os_error(libc::EBUSY as i32);
        let msg = format_worktree_path_io_message("a.txt", WorktreePathOperation::Read, &err);
        assert!(msg.contains("in use by another process"));
    }

    #[test]
    fn other_io_keeps_source_kind() {
        let err = io::Error::new(io::ErrorKind::NotFound, "gone");
        let msg = format_worktree_path_io_message("x", WorktreePathOperation::Remove, &err);
        assert!(msg.contains("cannot remove 'x'"));
        assert!(msg.contains("gone"));
    }
}
