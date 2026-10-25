//! Worktree path I/O errors name the path and stay actionable (issue #1915).

use grit_lib::error::Error;
use grit_lib::worktree::{format_worktree_path_io_message, map_worktree_io, WorktreePathOperation};
use std::io;

#[test]
fn file_in_use_message_names_path_without_raw_os_error() {
    #[cfg(windows)]
    let source = io::Error::from_raw_os_error(32);
    #[cfg(not(windows))]
    let source = io::Error::from_raw_os_error(libc::EBUSY as i32);
    let err = map_worktree_io("b.txt", WorktreePathOperation::Replace, source);
    let msg = err.to_string();
    assert!(msg.contains("b.txt"));
    assert!(msg.contains("in use by another process"));
    assert!(!msg.contains("os error"));
    assert!(matches!(err, Error::WorktreePathIo { .. }));
}

#[test]
fn format_helper_matches_error_display() {
    let source = io::Error::from_raw_os_error(libc::EBUSY as i32);
    let msg = format_worktree_path_io_message("a.txt", WorktreePathOperation::Read, &source);
    let err = map_worktree_io("a.txt", WorktreePathOperation::Read, source);
    assert_eq!(msg, err.to_string());
}
