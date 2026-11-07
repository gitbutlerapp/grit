//! Worktree path I/O errors name the path and stay actionable (issue #1915).

use grit_lib::error::{Error, PathIoError, PathIoOperation, WorktreePathOperation};
use grit_lib::worktree::WorktreeError;
use std::io;

fn worktree_path_io(path: &str, operation: WorktreePathOperation, source: io::Error) -> Error {
    PathIoError::worktree(path, operation, source).into()
}

#[test]
fn file_in_use_message_names_path_without_raw_os_error() {
    #[cfg(windows)]
    let source = io::Error::from_raw_os_error(32);
    #[cfg(not(windows))]
    let source = io::Error::from_raw_os_error(libc::EBUSY as i32);
    let err = worktree_path_io("b.txt", WorktreePathOperation::Replace, source);
    let msg = err.to_string();
    assert!(msg.contains("b.txt"));
    assert!(msg.contains("in use by another process"));
    assert!(!msg.contains("os error"));
    assert!(matches!(
        err,
        Error::Worktree(WorktreeError::PathIo(PathIoError {
            path: Some(ref path),
            operation: PathIoOperation::Worktree(WorktreePathOperation::Replace),
            ..
        })) if path.as_path() == std::path::Path::new("b.txt")
    ));
}

#[test]
fn non_sharing_io_error_includes_path_and_source() {
    let source = io::Error::new(io::ErrorKind::NotFound, "gone");
    let err = worktree_path_io("x", WorktreePathOperation::Remove, source);
    let msg = err.to_string();
    assert!(msg.contains("cannot remove 'x'"));
    assert!(msg.contains("gone"));
}
