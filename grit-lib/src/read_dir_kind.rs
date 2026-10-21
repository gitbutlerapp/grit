//! Directory entry type from `readdir` without resolving the full path when possible.
//!
//! Prefer [`DirEntry::file_type`] over [`DirEntry::path`] followed by `is_dir()`, which
//! forces a separate metadata lookup per entry on most platforms.

use std::fs::{DirEntry, FileType, Metadata};
use std::io;

/// Whether `entry` names a directory.
///
/// Uses [`DirEntry::file_type`], which reads `d_type` from `readdir` when the filesystem
/// provides it and only falls back to `stat` when the type is unknown.
pub(crate) fn entry_is_dir(entry: &DirEntry) -> io::Result<bool> {
    entry.file_type().map(|ft| ft.is_dir())
}

/// Whether Git would silently ignore this directory entry (FIFO, socket, device node, …).
///
/// Regular files, symlinks, and directories are not special; everything else is skipped for
/// untracked scans and staging, matching Git's worktree indexing rules.
pub(crate) fn entry_is_git_ignored_special(entry: &DirEntry) -> io::Result<bool> {
    entry
        .file_type()
        .map(|ft| file_type_is_git_ignored_special(&ft))
}

/// Same as [`entry_is_git_ignored_special`] for [`Metadata`] from `symlink_metadata`.
pub(crate) fn metadata_is_git_ignored_special(meta: &Metadata) -> bool {
    file_type_is_git_ignored_special(&meta.file_type())
}

fn file_type_is_git_ignored_special(ft: &FileType) -> bool {
    if ft.is_dir() || ft.is_file() || ft.is_symlink() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_fifo() || ft.is_socket() || ft.is_block_device() || ft.is_char_device() {
            return true;
        }
    }
    // Unknown readdir types (or non-indexable nodes on other platforms) are skipped like Git.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn regular_file_is_not_special() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("f.txt");
        fs::write(&path, b"x").expect("write");
        let entry = fs::read_dir(dir.path())
            .expect("read_dir")
            .find(|e| e.as_ref().ok().is_some_and(|e| e.file_name() == "f.txt"))
            .expect("entry")
            .expect("entry ok");
        assert!(!entry_is_git_ignored_special(&entry).expect("type"));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_special() {
        use std::process::Command;
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe");
        let status = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(status.success(), "mkfifo failed");
        let entry = fs::read_dir(dir.path())
            .expect("read_dir")
            .find(|e| e.as_ref().ok().is_some_and(|e| e.file_name() == "pipe"))
            .expect("entry")
            .expect("entry ok");
        assert!(entry_is_git_ignored_special(&entry).expect("type"));
        let meta = fs::symlink_metadata(&fifo).expect("stat");
        assert!(metadata_is_git_ignored_special(&meta));
    }
}
