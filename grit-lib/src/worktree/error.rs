//! Worktree-area failures: checkout, paths, pathspecs, filters, and linked worktrees.

pub use super::linked_worktree_error::{LinkedWorktreeError, LinkedWorktreeOperation};

use crate::error::{
    AttrPathspecError, Error, FilterError, OverwriteReason, PathOutsideRepoError,
    PathspecFromFileParseError, SubmoduleError, WorktreeOperation as DirtyWorktreeOperation,
};

/// Worktree-area failure (checkout, status, add/rm/mv/restore/clean, pathspec, filters, linked worktrees).
///
/// Surfaced on the umbrella as [`Error::Worktree`](crate::error::Error::Worktree).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorktreeError {
    /// Linked worktree registry add/remove/move/lock/repair failure.
    #[error(transparent)]
    Linked(#[from] LinkedWorktreeError),

    /// Filesystem I/O on a repository-relative worktree path.
    #[error("{0}")]
    PathIo(#[from] crate::error::PathIoError),

    /// A worktree file changed while it was being hashed and stored.
    #[error("{path} changed while it was being added")]
    FileChanged {
        /// Index-relative path of the file.
        path: String,
    },

    /// General path resolution failure (invalid UTF-8, out-of-bounds, etc.).
    #[error("path error: {detail}")]
    Path {
        /// What went wrong with the path.
        detail: String,
    },

    /// A path string failed validation (legacy invalid-path checks).
    #[error("invalid path '{path}'")]
    InvalidPath {
        /// Offending path text.
        path: String,
    },

    /// A repository-relative path failed component validation.
    #[error("invalid path '{path}': {reason}")]
    InvalidPathComponent {
        /// Full repository-relative path that was rejected.
        path: String,
        /// Which component rule failed.
        reason: crate::path_validate::PathComponentError,
    },

    /// Path resolves outside the repository.
    #[error(transparent)]
    OutsideRepo(#[from] PathOutsideRepoError),

    /// `--pathspec-from-file` parse failure.
    #[error(transparent)]
    PathspecFromFile(#[from] PathspecFromFileParseError),

    /// Malformed `:(attr:...)` pathspec magic.
    #[error("invalid attr magic in pathspec '{spec}': {reason}")]
    InvalidAttrPathspec {
        /// The offending pathspec as given.
        spec: String,
        /// What is wrong with its attr clause.
        #[source]
        reason: AttrPathspecError,
    },

    /// Pathspec matched no tracked or indexed file.
    #[error("pathspec '{spec}' did not match any file(s) known to git")]
    PathspecNoMatch {
        /// User-supplied pathspec that matched nothing.
        spec: String,
    },

    /// Literal pathspec crosses a symlink directory component.
    #[error("pathspec '{spec}' is beyond a symbolic link")]
    PathspecBeyondSymlink {
        /// User-supplied pathspec that crosses a symlink directory.
        spec: String,
    },

    /// Explicit pathspecs name ignored paths.
    #[error("the following paths are ignored:\n{}", paths.join("\n"))]
    PathspecIgnored {
        /// Repository-relative paths that cannot be staged while ignored.
        paths: Vec<String>,
    },

    /// Restore conflict options combined with `--staged`.
    #[error("conflict restore options cannot be used with --staged")]
    RestoreConflictWithStaged,

    /// Restore conflict options combined with `--source`.
    #[error("conflict restore options cannot be used with --source")]
    RestoreConflictWithSource,

    /// Missing "theirs" stage for a conflict restore path.
    #[error("path '{path}' does not have their version")]
    RestoreConflictMissingTheirs {
        /// Repository-relative path.
        path: String,
    },

    /// Missing "ours" stage for a conflict restore path.
    #[error("path '{path}' does not have our version")]
    RestoreConflictMissingOurs {
        /// Repository-relative path.
        path: String,
    },

    /// Path is not unmerged when remerge was requested.
    #[error("path '{path}' is not unmerged")]
    RestoreConflictNotUnmerged {
        /// Repository-relative path.
        path: String,
    },

    /// Worktree restore requested for paths that still have conflict stages.
    #[error("{}", crate::error::restore_unmerged_message(paths))]
    RestorePathUnmerged {
        /// Matched repository-relative paths with conflict stages (sorted, deduplicated).
        paths: Vec<String>,
    },

    /// Nested `.git` checkout has no commit on its current branch.
    #[error("embedded git repository '{path}' has no commit checked out")]
    EmbeddedRepoWithoutCommit {
        /// Repository-relative path to the nested checkout directory.
        path: String,
    },

    /// Remove refused paths with unsaved content without `--force`.
    #[error(
        "refusing to remove paths with unsaved content:\n{}",
        paths.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    )]
    RemoveBlocked {
        /// Each refused path with the reason it was refused.
        paths: Vec<crate::porcelain::paths::BlockedRemoval>,
    },

    /// Directory pathspec without `-r` on remove.
    #[error("not removing '{path}' recursively without -r")]
    NotRemovingRecursively {
        /// Directory path that requires `-r`.
        path: String,
    },

    /// Remove called with no pathspecs.
    #[error("you must specify path(s) to remove")]
    RemovePathsRequired,

    /// Operation requires a checked-out work tree.
    #[error("this operation must be run in a work tree")]
    RequiresWorkTree,

    /// Move source is not tracked.
    #[error("'{src}' is not under version control")]
    MoveSourceNotTracked {
        /// Untracked or missing source path.
        src: String,
    },

    /// Move destination exists without `--force`.
    #[error("destination '{destination}' already exists")]
    MoveDestinationExists {
        /// Source path in the move request.
        src: String,
        /// Existing destination path.
        destination: String,
    },

    /// Trailing slash named a destination that is not a directory.
    #[error("destination '{destination}' is not a directory")]
    MoveDestinationNotDirectory {
        /// Destination path (trailing slash already removed).
        destination: String,
    },

    /// Destination lies under the source directory.
    #[error("cannot move '{src}' to a subdirectory of itself, '{destination}'")]
    MoveIntoSubdirectory {
        /// Source directory path.
        src: String,
        /// Invalid destination under the source.
        destination: String,
    },

    /// Source and destination are the same path.
    #[error("cannot move '{src}' into itself")]
    MoveIntoSelf {
        /// Source path.
        src: String,
        /// Destination equal to the source.
        destination: String,
    },

    /// Source has no index entries to move.
    #[error("nothing to move from '{src}'")]
    MoveNothingToMove {
        /// Source path with no index entries.
        src: String,
    },

    /// Multiple sources would move to the same destination.
    #[error("multiple sources would move to '{destination}'")]
    MoveDuplicateDestination {
        /// Destination path that would receive more than one source.
        destination: String,
    },

    /// Move source path is missing or not a file, directory, or symlink.
    #[error("cannot move '{src}' to '{destination}': bad source path")]
    MoveBadSource {
        /// Source path.
        src: String,
        /// Planned destination path.
        destination: String,
    },

    /// Internal invariant: missing index entry during a move.
    #[error("internal error: missing index entry for '{path}'")]
    MoveMissingIndexEntry {
        /// Index path that should exist during a move.
        path: String,
    },

    /// Checkout or switch would overwrite local or untracked content.
    #[error("{message}")]
    WouldOverwrite {
        /// Whether tracked local edits or untracked paths block the operation.
        reason: OverwriteReason,
        /// Repository-relative paths that would be overwritten (sorted, deduplicated).
        paths: Vec<String>,
        /// Human-readable summary (depends on [`OverwriteReason`]).
        message: String,
    },

    /// Staged or unstaged changes block an operation that needs a clean worktree.
    #[error("you have uncommitted changes — commit them before {operation}")]
    DirtyWorktree {
        /// The operation that was refused.
        operation: DirtyWorktreeOperation,
        /// Repository-relative paths with staged or unstaged changes (sorted, deduplicated).
        paths: Vec<String>,
    },

    /// Checkout stopped before completion and rollback left paths unrestored.
    #[error("{source}; {} work-tree path(s) could not be restored", unrestored.len())]
    CheckoutRollbackIncomplete {
        /// Why the checkout stopped.
        source: Box<Error>,
        /// Repository-relative paths left different from the index (sorted, deduplicated).
        unrestored: Vec<String>,
    },

    /// Staging refused paths that still contain conflict markers.
    #[error("unmerged paths still have conflict markers")]
    UnmergedConflictMarkers {
        /// Repository-relative paths that were not staged because markers remain.
        paths: Vec<String>,
    },

    /// Clean/smudge filter or EOL conversion failure.
    #[error(transparent)]
    Filter(#[from] FilterError),

    /// Submodule layout or state failure during worktree operations.
    #[error(transparent)]
    Submodule(#[from] SubmoduleError),
}

impl WorktreeError {
    /// Stable snake_case slug for this failure (matches former [`Error::kind_name`] values).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Linked(_) => "worktree",
            Self::PathIo(path_io) => match path_io.operation {
                crate::error::PathIoOperation::Worktree(_) => "worktree_path_io",
                crate::error::PathIoOperation::GitDir(_) => "git_dir_path_io",
            },
            Self::FileChanged { .. } => "worktree_file_changed",
            Self::Path { .. } => "path_error",
            Self::InvalidPath { .. } => "invalid_path",
            Self::InvalidPathComponent { .. } => "invalid_path_component",
            Self::OutsideRepo(_) => "path_outside_repo",
            Self::PathspecFromFile(_) => "pathspec_from_file",
            Self::InvalidAttrPathspec { .. } => "invalid_attr_pathspec",
            Self::PathspecNoMatch { .. } => "pathspec_no_match",
            Self::PathspecBeyondSymlink { .. } => "pathspec_beyond_symlink",
            Self::PathspecIgnored { .. } => "pathspec_ignored",
            Self::RestoreConflictWithStaged => "restore_conflict_with_staged",
            Self::RestoreConflictWithSource => "restore_conflict_with_source",
            Self::RestoreConflictMissingTheirs { .. } => "restore_conflict_missing_theirs",
            Self::RestoreConflictMissingOurs { .. } => "restore_conflict_missing_ours",
            Self::RestoreConflictNotUnmerged { .. } => "restore_conflict_not_unmerged",
            Self::RestorePathUnmerged { .. } => "restore_path_unmerged",
            Self::EmbeddedRepoWithoutCommit { .. } => "embedded_repo_without_commit",
            Self::RemoveBlocked { .. } => "remove_blocked",
            Self::NotRemovingRecursively { .. } => "not_removing_recursively",
            Self::RemovePathsRequired => "remove_paths_required",
            Self::RequiresWorkTree => "requires_work_tree",
            Self::MoveSourceNotTracked { .. } => "move_source_not_tracked",
            Self::MoveDestinationExists { .. } => "move_destination_exists",
            Self::MoveDestinationNotDirectory { .. } => "move_destination_not_directory",
            Self::MoveIntoSubdirectory { .. } => "move_into_subdirectory",
            Self::MoveIntoSelf { .. } => "move_into_self",
            Self::MoveNothingToMove { .. } => "move_nothing_to_move",
            Self::MoveDuplicateDestination { .. } => "move_duplicate_destination",
            Self::MoveBadSource { .. } => "move_bad_source",
            Self::MoveMissingIndexEntry { .. } => "move_missing_index_entry",
            Self::WouldOverwrite { .. } => "would_overwrite",
            Self::DirtyWorktree { .. } => "dirty_worktree",
            Self::CheckoutRollbackIncomplete { .. } => "checkout_rollback_incomplete",
            Self::UnmergedConflictMarkers { .. } => "unmerged_conflict_markers",
            Self::Filter(_) => "filter",
            Self::Submodule(_) => "submodule",
        }
    }
}
