//! Map filesystem failures under the repository git directory to [`Error::GitDirPathIo`].

use std::io;
use std::path::Path;

use crate::error::{Error, Result};

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

/// Repository-relative display path for a file under the git directory (e.g. `.git/objects`).
#[must_use]
pub fn display_git_dir_path(git_dir: &Path, path: &Path) -> String {
    if let Some(common) = crate::refs::common_dir(git_dir) {
        if let Some(repo_root) = common.parent() {
            if let Ok(rel) = path.strip_prefix(repo_root) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
    }
    if let Ok(rel) = path.strip_prefix(git_dir) {
        let suffix = rel.to_string_lossy();
        if suffix.is_empty() {
            return ".git".to_owned();
        }
        return format!(".git/{}", suffix.trim_start_matches('/'));
    }
    if let Some(worktree) = git_dir.parent() {
        if let Ok(rel) = path.strip_prefix(worktree) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    path.display().to_string()
}

/// Human-readable summary for [`Error::GitDirPathIo`].
#[must_use]
pub fn format_git_dir_io_message(
    path: &str,
    operation: GitDirPathOperation,
    source: &io::Error,
) -> String {
    if source.kind() == io::ErrorKind::PermissionDenied {
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
    match err {
        Error::Io(source) => {
            map_git_dir_io(git_dir, index_path, GitDirPathOperation::WriteIndex, source)
        }
        other => other,
    }
}

/// Like [`map_git_dir_io`], inferring the git directory from an `objects/` root.
#[must_use]
pub fn map_objects_dir_io(
    objects_dir: &Path,
    path: &Path,
    operation: GitDirPathOperation,
    source: io::Error,
) -> Error {
    let git_dir = objects_dir.parent().unwrap_or(objects_dir);
    map_git_dir_io(git_dir, path, operation, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn display_path_under_git_dir() {
        let git = PathBuf::from("/tmp/pe/.git");
        let path = git.join("objects/ab/cdef");
        assert_eq!(display_git_dir_path(&git, &path), ".git/objects/ab/cdef");
    }

    #[test]
    fn display_path_linked_worktree_admin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path().join("pe");
        let common = repo_root.join(".git");
        let linked = common.join("worktrees/wt1");
        fs::create_dir_all(&linked).expect("mkdir");
        fs::write(linked.join("commondir"), "../..\n").expect("commondir");
        let path = linked.join("index");
        assert_eq!(
            display_git_dir_path(&linked, &path),
            ".git/worktrees/wt1/index"
        );
    }

    #[test]
    fn permission_message_names_git_path() {
        let err = io::Error::from_raw_os_error(libc::EACCES as i32);
        let msg = format_git_dir_io_message(".git/objects", GitDirPathOperation::WriteObject, &err);
        assert!(msg.contains(".git/objects"));
        assert!(msg.contains("insufficient permission"));
    }
}
