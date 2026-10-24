//! Typed failures for linked worktree add, remove, move, lock and repair.

use std::fmt;
use std::path::PathBuf;

use crate::git_path::user_display_path;

/// Linked worktree mutation that was pointed at the main working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeOperation {
    /// [`remove_linked_worktree`](crate::worktree::remove_linked_worktree).
    Remove,
    /// [`move_linked_worktree`](crate::worktree::move_linked_worktree).
    Move,
    /// [`RepositoryWorktrees::lock`](crate::repo_handles::RepositoryWorktrees::lock).
    Lock,
    /// [`RepositoryWorktrees::unlock`](crate::repo_handles::RepositoryWorktrees::unlock).
    Unlock,
}

impl WorktreeOperation {
    /// Lowercase verb for the operation (`remove`, `move`, `lock`, `unlock`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remove => "remove",
            Self::Move => "move",
            Self::Lock => "lock",
            Self::Unlock => "unlock",
        }
    }
}

impl fmt::Display for WorktreeOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Linked worktree failures, wrapped by [`Error::Worktree`](crate::error::Error::Worktree).
///
/// Branch problems met while adding a worktree (invalid, missing or existing branch names)
/// are reported as [`BranchError`](crate::porcelain::branch_error::BranchError) instead, and a
/// branch held by another worktree as
/// [`Error::BranchCheckedOutInWorktree`](crate::error::Error::BranchCheckedOutInWorktree).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WorktreeError {
    /// Linked worktrees cannot be added to a bare repository.
    #[error("cannot add a worktree to a bare repository")]
    BareRepository,

    /// No registered worktree has `path` as its working tree root.
    #[error("'{}' is not a registered worktree", user_display_path(.path))]
    NotRegistered {
        /// Normalized path that matched no registry entry.
        path: PathBuf,
    },

    /// The operation only applies to linked worktrees.
    #[error("cannot {operation} the main working tree")]
    MainWorktree {
        /// Operation that was refused.
        operation: WorktreeOperation,
    },

    /// The worktree is locked and the caller did not force the operation.
    #[error("{}", with_reason("worktree is locked", .reason))]
    Locked {
        /// Contents of the `locked` file, when non-empty.
        reason: Option<String>,
    },

    /// Lock was requested for a worktree that is already locked.
    #[error("{}", with_reason("worktree is already locked", .reason))]
    AlreadyLocked {
        /// Contents of the existing `locked` file, when non-empty.
        reason: Option<String>,
    },

    /// Unlock was requested for a worktree that is not locked.
    #[error("worktree is not locked")]
    NotLocked,

    /// Remove without force found staged, unstaged, conflicted or untracked changes.
    #[error("worktree contains modified or untracked files; use -f or --force to remove")]
    HasLocalChanges,

    /// Move found populated submodules inside the worktree.
    #[error("working trees containing submodules cannot be moved or removed")]
    HasSubmodules,

    /// The path is registered to a worktree whose directory still exists.
    #[error("'{}' is already a registered worktree", user_display_path(.path))]
    PathRegistered {
        /// Registered working tree path.
        path: PathBuf,
    },

    /// The path is registered to a worktree whose directory is missing, and add was not forced.
    #[error(
        "'{}' is a missing but already registered worktree; use add with --force to override, or prune or remove to clear",
        user_display_path(.path)
    )]
    PathRegisteredMissing {
        /// Registered working tree path that is missing on disk.
        path: PathBuf,
    },

    /// The target directory exists and is not empty (add) or exists at all (move).
    #[error("'{}' already exists", user_display_path(.path))]
    PathExists {
        /// Occupied path.
        path: PathBuf,
    },

    /// The add target exists and is a file or other non-directory.
    #[error("'{}' already exists and is not a directory", user_display_path(.path))]
    PathExistsNotDirectory {
        /// Occupied path.
        path: PathBuf,
    },

    /// A path that must be a directory (repair target, move destination parent) is not one.
    #[error("'{}' is not a directory", user_display_path(.path))]
    NotADirectory {
        /// Offending path.
        path: PathBuf,
    },

    /// A repair target does not exist.
    #[error("'{}' is not a valid path", user_display_path(.path))]
    PathNotFound {
        /// Missing path.
        path: PathBuf,
    },

    /// The path has no final component to name the worktree after (for example `/`).
    #[error("invalid worktree path '{}'", user_display_path(.path))]
    NoFileName {
        /// Path without a final component.
        path: PathBuf,
    },

    /// A worktree admin directory has no parent directory.
    #[error("invalid worktree admin directory '{}'", user_display_path(.path))]
    InvalidAdminDir {
        /// Admin directory path.
        path: PathBuf,
    },

    /// An existing branch used for the worktree `HEAD` does not point at the start commit.
    #[error("branch '{branch}' does not match start commit")]
    BranchTipMismatch {
        /// Short branch name.
        branch: String,
    },

    /// Repair expected a `.git` gitfile but found a directory.
    #[error("'{}' is a directory, not a worktree gitfile", user_display_path(.path))]
    DotGitIsDirectory {
        /// The worktree's `.git` path.
        path: PathBuf,
    },

    /// Repair could not map a worktree's `.git` gitfile to a registered admin directory.
    #[error("'{}' does not point at a registered worktree", user_display_path(.path))]
    BrokenGitFile {
        /// The worktree's `.git` path.
        path: PathBuf,
    },

    /// Another process holds the lock file guarding a worktree link file.
    #[error("could not acquire lock for '{}'", user_display_path(.path))]
    LockHeld {
        /// Lock file path.
        path: PathBuf,
    },

    /// A gitlink entry in the worktree index has a path that is not valid UTF-8.
    #[error(
        "submodule path in index is not valid UTF-8: {}",
        String::from_utf8_lossy(.path)
    )]
    NonUtf8SubmodulePath {
        /// Raw index path bytes.
        path: Vec<u8>,
    },
}

fn with_reason(prefix: &str, reason: &Option<String>) -> String {
    match reason {
        Some(r) => format!("{prefix}: {r}"),
        None => prefix.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_messages_include_reason_when_present() {
        assert_eq!(
            WorktreeError::Locked { reason: None }.to_string(),
            "worktree is locked"
        );
        assert_eq!(
            WorktreeError::AlreadyLocked {
                reason: Some("hold".into())
            }
            .to_string(),
            "worktree is already locked: hold"
        );
    }

    #[test]
    fn main_worktree_message_names_operation() {
        let msgs: Vec<String> = [
            WorktreeOperation::Remove,
            WorktreeOperation::Move,
            WorktreeOperation::Lock,
            WorktreeOperation::Unlock,
        ]
        .into_iter()
        .map(|operation| WorktreeError::MainWorktree { operation }.to_string())
        .collect();
        assert_eq!(
            msgs,
            [
                "cannot remove the main working tree",
                "cannot move the main working tree",
                "cannot lock the main working tree",
                "cannot unlock the main working tree",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_variants_display_the_path() {
        let path = PathBuf::from("/tmp/wt");
        let err = WorktreeError::NotRegistered { path: path.clone() };
        assert_eq!(err.to_string(), "'/tmp/wt' is not a registered worktree");
        let err = WorktreeError::PathExists { path };
        assert_eq!(err.to_string(), "'/tmp/wt' already exists");
    }
}
