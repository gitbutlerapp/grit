//! Directory entry type from `readdir` without resolving the full path when possible.
//!
//! Prefer [`DirEntry::file_type`] over [`DirEntry::path`] followed by `is_dir()`, which
//! forces a separate metadata lookup per entry on most platforms.

use std::fs::DirEntry;
use std::io;

/// Whether `entry` names a directory.
///
/// Uses [`DirEntry::file_type`], which reads `d_type` from `readdir` when the filesystem
/// provides it and only falls back to `stat` when the type is unknown.
pub(crate) fn entry_is_dir(entry: &DirEntry) -> io::Result<bool> {
    entry.file_type().map(|ft| ft.is_dir())
}
