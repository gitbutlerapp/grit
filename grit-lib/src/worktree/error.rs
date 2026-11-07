//! Worktree-area failures: checkout, paths, pathspecs, filters, and linked worktrees.

pub use super::linked_worktree_error::{LinkedWorktreeError, LinkedWorktreeOperation};

#[cfg(doc)]
use crate::error::Error;
use crate::error::{
    AttrPathspecError, ErrorSummary, FilterError, OverwriteReason, PathOutsideRepoError,
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

    /// A path that must be UTF-8 for this operation is not.
    #[error("path is not valid UTF-8: {path}")]
    NonUtf8Path {
        /// The path with invalid bytes replaced.
        path: String,
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

    /// Clean refused to remove a directory that contains the current working directory.
    #[error("refusing to remove '{path}' because it contains the current directory")]
    RemoveContainsCurrentDirectory {
        /// Repository-relative directory path.
        path: String,
    },

    /// Restore called with no pathspecs.
    #[error("you must specify path(s) to restore")]
    RestorePathsRequired,

    /// Restore was asked both to pick a conflict side and to rebuild conflict markers.
    #[error("cannot combine choosing a conflict side with rebuilding the conflict")]
    RestoreStageWithRemerge,

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

    /// A checkout failed or was cancelled before it finished, and putting the work tree back
    /// failed for some paths.
    ///
    /// Refs and the index are unchanged, as after any failed checkout; only `unrestored`
    /// differ from the index. [`Error::is_cancelled`] reports whether `source` was a
    /// cancellation.
    #[error("{source}; {} work-tree path(s) could not be restored", unrestored.len())]
    CheckoutRollbackIncomplete {
        /// Why the checkout stopped.
        source: ErrorSummary,
        /// Repository-relative paths left different from the index (sorted, deduplicated).
        unrestored: Vec<String>,
    },

    /// A move failed, and putting the work tree back failed too.
    ///
    /// The index is unchanged; renames that could not be reversed and forced-destination
    /// backups that could not be restored are left as they are on disk.
    #[error("{}", move_rollback_incomplete_message(source, *unreversed, backup_restore.as_ref()))]
    MoveRollbackIncomplete {
        /// Why the move stopped.
        source: ErrorSummary,
        /// Number of completed renames that could not be reversed.
        unreversed: usize,
        /// Why restoring the destinations a forced move replaced failed, when it did.
        backup_restore: Option<ErrorSummary>,
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
    /// Whether this failure records a cancellation (a checkout whose rollback was incomplete
    /// after it was cancelled).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::CheckoutRollbackIncomplete { source, .. }
            | Self::MoveRollbackIncomplete { source, .. } => source.is_cancelled(),
            _ => false,
        }
    }

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
            Self::NonUtf8Path { .. } => "non_utf8_path",
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
            Self::RemoveContainsCurrentDirectory { .. } => "remove_contains_current_directory",
            Self::RestorePathsRequired => "restore_paths_required",
            Self::RestoreStageWithRemerge => "restore_stage_with_remerge",
            Self::MoveRollbackIncomplete { .. } => "move_rollback_incomplete",
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

fn move_rollback_incomplete_message(
    source: &ErrorSummary,
    unreversed: usize,
    backup_restore: Option<&ErrorSummary>,
) -> String {
    let mut message = source.to_string();
    if unreversed > 0 {
        message.push_str(&format!(
            "; incomplete worktree rollback ({unreversed} rename(s) could not be reversed)"
        ));
    }
    if let Some(err) = backup_restore {
        message.push_str(&format!(
            "; restoring forced-destination backups failed: {err}"
        ));
    }
    message
}
