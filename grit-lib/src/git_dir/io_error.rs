//! Map filesystem failures under the repository git directory to [`Error::GitDirPathIo`].

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::objects::odb::Odb;

/// Git-directory filesystem operation that failed on a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GitDirPathOperation {
    /// Writing a loose object or object store directory under `objects/`.
    WriteObject,
    /// Creating or updating a ref file or lock under `refs/`.
    UpdateRef,
    /// Appending to a reflog file under `logs/`.
    AppendReflog,
    /// Writing or replacing the index file.
    WriteIndex,
    /// Repack, prune, or other maintenance writing under `objects/pack/`.
    GcPack,
    /// Creating a directory under the git dir (refs, logs, objects, …).
    CreateDir,
}

impl GitDirPathOperation {
    fn phrase(self) -> &'static str {
        match self {
            Self::WriteObject => "add an object to the object database",
            Self::UpdateRef => "update the ref store",
            Self::AppendReflog => "append to the reflog",
            Self::WriteIndex => "write the index",
            Self::GcPack => "run repository maintenance",
            Self::CreateDir => "create a directory in the repository",
        }
    }
}

/// True when `git_dir` is the hidden `.git` directory of a linked worktree.
fn layout_uses_dot_git_prefix(git_dir: &Path) -> bool {
    git_dir.file_name().is_some_and(|n| n == ".git")
}

/// Normalize a path suffix from [`Path::strip_prefix`] for stable `.git/…` display (forward slashes).
fn normalize_repo_relative_suffix(rel: &str) -> String {
    rel.trim_start_matches(['/', '\\']).replace('\\', "/")
}

/// Format a repository-relative suffix for display (worktree vs bare).
fn format_layout_path(layout_git_dir: &Path, rel: &str) -> String {
    if rel.is_empty() {
        if layout_uses_dot_git_prefix(layout_git_dir) {
            return ".git".to_owned();
        }
        return layout_git_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".into());
    }
    if layout_uses_dot_git_prefix(layout_git_dir) {
        format!(".git/{rel}")
    } else {
        rel.to_owned()
    }
}

/// Display path for ref-store I/O under `common_dir` and the checkout's `git_dir` (linked admin dir).
#[must_use]
pub fn display_ref_store_path(common_dir: &Path, checkout_git_dir: &Path, path: &Path) -> String {
    let layout = if layout_uses_dot_git_prefix(common_dir) {
        common_dir
    } else {
        checkout_git_dir
    };
    let rel = if let Ok(r) = path.strip_prefix(common_dir) {
        normalize_repo_relative_suffix(&r.to_string_lossy())
    } else if let Ok(tail) = path.strip_prefix(checkout_git_dir) {
        let middle = checkout_git_dir
            .strip_prefix(common_dir)
            .map(|p| normalize_repo_relative_suffix(&p.to_string_lossy()))
            .unwrap_or_default();
        let tail = normalize_repo_relative_suffix(&tail.to_string_lossy());
        if middle.is_empty() {
            tail
        } else {
            format!("{middle}/{tail}")
        }
    } else {
        return path.display().to_string();
    };
    format_layout_path(layout, &rel)
}

/// Repository-relative display path for a file under the git directory (e.g. `.git/objects`).
#[must_use]
pub fn display_git_dir_path(git_dir: &Path, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(git_dir) {
        let trimmed = normalize_repo_relative_suffix(&rel.to_string_lossy());
        return format_layout_path(git_dir, &trimmed);
    }
    if layout_uses_dot_git_prefix(git_dir) {
        if let Some(worktree) = git_dir.parent() {
            if let Ok(rel) = path.strip_prefix(worktree) {
                return normalize_repo_relative_suffix(&rel.to_string_lossy());
            }
        }
    }
    path.display().to_string()
}

/// Resolve the git directory used for pack/maintenance I/O messages.
#[must_use]
pub fn git_dir_for_pack_io(odb: &Odb, pack_area_path: &Path) -> PathBuf {
    if let Some(git_dir) = odb.config_git_dir() {
        return git_dir.to_path_buf();
    }
    pack_area_path
        .parent()
        .and_then(|objects| objects.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| pack_area_path.to_path_buf())
}

/// Git directory inferred from an `objects/` directory path.
#[must_use]
pub fn git_dir_from_objects_dir(objects_dir: &Path) -> PathBuf {
    objects_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| objects_dir.to_path_buf())
}

/// Git directory inferred from `objects/pack/` (or a file inside it).
#[must_use]
pub fn git_dir_from_pack_dir(pack_dir: &Path) -> PathBuf {
    pack_dir
        .parent()
        .and_then(|objects| objects.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| pack_dir.to_path_buf())
}

/// Whether a git-dir I/O failure is likely transient contention (retry), not a permanent ACL.
#[must_use]
pub fn is_git_dir_transient_contention(operation: GitDirPathOperation, source: &io::Error) -> bool {
    if crate::worktree::io_error::is_file_in_use(source) {
        return true;
    }
    matches!(
        operation,
        GitDirPathOperation::WriteIndex | GitDirPathOperation::UpdateRef
    ) && crate::atomic_replace::is_replace_contention(source)
}

/// Human-readable summary for [`Error::GitDirPathIo`].
#[must_use]
pub fn format_git_dir_io_message(
    path: &str,
    operation: GitDirPathOperation,
    source: &io::Error,
) -> String {
    if is_git_dir_transient_contention(operation, source) {
        format!(
            "cannot {} ({path}): file is in use by another process (close it and retry)",
            operation.phrase()
        )
    } else if source.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "insufficient permission to {} ({path}): {source}",
            operation.phrase()
        )
    } else {
        format!("cannot {} ({path}): {source}", operation.phrase())
    }
}

/// Wrap a raw I/O failure with git-dir path and operation context.
#[must_use]
pub fn map_git_dir_io(
    git_dir: &Path,
    path: &Path,
    operation: GitDirPathOperation,
    source: io::Error,
) -> Error {
    Error::GitDirPathIo {
        path: display_git_dir_path(git_dir, path),
        operation,
        source,
    }
}

/// Like [`map_git_dir_io`], for the files ref store (`common_dir` + checkout `git_dir`).
#[must_use]
pub fn map_ref_store_path_io(
    common_dir: &Path,
    checkout_git_dir: &Path,
    path: &Path,
    operation: GitDirPathOperation,
    source: io::Error,
) -> Error {
    Error::GitDirPathIo {
        path: display_ref_store_path(common_dir, checkout_git_dir, path),
        operation,
        source,
    }
}

/// Attach git-dir context to [`std::io::Result`].
pub fn io_with_git_path<T>(
    git_dir: &Path,
    path: &Path,
    operation: GitDirPathOperation,
    result: io::Result<T>,
) -> Result<T> {
    result.map_err(|source| map_git_dir_io(git_dir, path, operation, source))
}

/// Map a bare [`Error::Io`] from index persistence to [`Error::GitDirPathIo`].
#[must_use]
pub fn map_index_write_error(git_dir: &Path, index_path: &Path, err: Error) -> Error {
    let display_path = if let Some(common) = crate::refs::common_dir(git_dir) {
        display_ref_store_path(&common, git_dir, index_path)
    } else {
        display_git_dir_path(git_dir, index_path)
    };
    match err {
        Error::Io(source) => Error::GitDirPathIo {
            path: display_path,
            operation: GitDirPathOperation::WriteIndex,
            source,
        },
        other => other,
    }
}

/// Attach ref-store git-dir context to [`std::io::Result`].
pub fn io_with_ref_store_path<T>(
    common_dir: &Path,
    checkout_git_dir: &Path,
    path: &Path,
    operation: GitDirPathOperation,
    result: io::Result<T>,
) -> Result<T> {
    result.map_err(|source| {
        map_ref_store_path_io(common_dir, checkout_git_dir, path, operation, source)
    })
}

/// Like [`map_git_dir_io`], inferring the git directory from an `objects/` root.
#[must_use]
pub fn map_objects_dir_io(
    objects_dir: &Path,
    path: &Path,
    operation: GitDirPathOperation,
    source: io::Error,
) -> Error {
    let git_dir = git_dir_from_objects_dir(objects_dir);
    map_git_dir_io(&git_dir, path, operation, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn display_path_under_dot_git() {
        let git = PathBuf::from("/tmp/pe/.git");
        let path = git.join("objects/ab/cdef");
        assert_eq!(display_git_dir_path(&git, &path), ".git/objects/ab/cdef");
    }

    #[test]
    fn strip_prefix_suffix_normalizes_windows_separators() {
        let git = PathBuf::from("/tmp/pe/.git");
        let rel = normalize_repo_relative_suffix(r"\objects\ab\cdef");
        assert_eq!(rel, "objects/ab/cdef");
        assert_eq!(format_layout_path(&git, &rel), ".git/objects/ab/cdef");
    }

    #[cfg(windows)]
    #[test]
    fn display_path_under_dot_git_on_windows() {
        let git = PathBuf::from(r"C:\Users\pe\repo\.git");
        let path = PathBuf::from(r"C:\Users\pe\repo\.git\objects\ab\cdef");
        let display = display_git_dir_path(&git, &path);
        assert_eq!(display, ".git/objects/ab/cdef");
        assert!(
            !display.contains('\\'),
            "git-dir display paths must use forward slashes: {display}"
        );
    }

    #[test]
    fn display_path_under_bare_repo() {
        let git = PathBuf::from("/tmp/foo.git");
        let path = git.join("objects/pack/tmp_repack_1");
        assert_eq!(
            display_git_dir_path(&git, &path),
            "objects/pack/tmp_repack_1"
        );
    }

    #[test]
    fn storage_path_uses_dot_git_for_worktree_layout() {
        let git = PathBuf::from("/tmp/wt/.git");
        let common = PathBuf::from("/shared/commondir");
        let path = common.join("refs/heads/main");
        assert_eq!(
            display_ref_store_path(&common, &git, &path),
            ".git/refs/heads/main"
        );
    }

    #[test]
    fn ref_store_path_for_linked_worktree_head_lock() {
        let common = PathBuf::from("/tmp/main/.git");
        let admin = common.join("worktrees/linked");
        let lock = admin.join("HEAD.lock");
        assert_eq!(
            display_ref_store_path(&common, &admin, &lock),
            ".git/worktrees/linked/HEAD.lock"
        );
    }

    #[test]
    fn ref_store_path_for_bare_pack_staging() {
        let bare = PathBuf::from("/tmp/repo.git");
        let path = bare.join("objects/pack/tmp_repack_1");
        assert_eq!(
            display_ref_store_path(&bare, &bare, &path),
            "objects/pack/tmp_repack_1"
        );
    }

    #[test]
    fn permission_message_names_git_path() {
        let err = io::Error::from_raw_os_error(libc::EACCES as i32);
        let msg = format_git_dir_io_message(".git/objects", GitDirPathOperation::WriteObject, &err);
        assert!(msg.contains(".git/objects"));
        assert!(msg.contains("insufficient permission"));
    }

    #[test]
    fn sharing_violation_maps_to_in_use_message() {
        let err = io::Error::from_raw_os_error(libc::EBUSY as i32);
        let msg = format_git_dir_io_message(".git/index", GitDirPathOperation::WriteIndex, &err);
        assert!(msg.contains("in use by another process"));
        assert!(!msg.contains("insufficient permission"));
    }

    #[cfg(windows)]
    #[test]
    fn access_denied_maps_to_in_use_for_index_write_only() {
        let err = io::Error::from_raw_os_error(5);
        let index = format_git_dir_io_message(".git/index", GitDirPathOperation::WriteIndex, &err);
        assert!(index.contains("in use by another process"));
        let object =
            format_git_dir_io_message(".git/objects", GitDirPathOperation::WriteObject, &err);
        assert!(object.contains("insufficient permission"));
    }
}
