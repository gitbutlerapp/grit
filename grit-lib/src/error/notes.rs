//! [`NotesError`]: notes ref, notes tree, and notes merge failures.

use thiserror::Error;

/// Notes failure: notes refs, the notes tree layout, rewrite configuration, and notes merges.
///
/// Surfaced on the umbrella as [`Error::Notes`](crate::error::Error::Notes).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NotesError {
    /// [`RepositoryNotes::add`](crate::repo_handles::RepositoryNotes::add) was called without
    /// `force` when a note already exists on the object.
    #[error("note for object {object} already exists")]
    AlreadyExists {
        /// Annotated object that already has a note.
        object: crate::objects::ObjectId,
    },

    /// A configured or environment notes ref is not under `refs/notes/`.
    #[error("notes ref '{name}' must start with 'refs/notes/'")]
    InvalidRef {
        /// The notes ref as configured.
        name: String,
    },

    /// Unknown `notes.rewriteMode` or `GIT_NOTES_REWRITE_MODE` value.
    #[error("unknown notes rewrite mode '{mode}'")]
    UnknownRewriteMode {
        /// The rewrite mode as configured.
        mode: String,
    },

    /// A notes ref resolves to an object that cannot hold notes (notes reads accept a commit or
    /// a tree; notes merges require a commit).
    #[error("notes ref '{notes_ref}' points to a {found}, not a notes commit")]
    RefNotCommit {
        /// Notes ref (or revision) that was read.
        notes_ref: String,
        /// Kind of object the ref resolved to.
        found: crate::objects::ObjectKind,
    },

    /// An object reached while reading a notes tree has the wrong kind (for example a note
    /// entry that is not a blob).
    #[error("notes object {oid} is a {found}, expected a {expected}")]
    UnexpectedObjectKind {
        /// Object that was read.
        oid: crate::objects::ObjectId,
        /// Kind the notes layout requires at this position.
        expected: crate::objects::ObjectKind,
        /// Kind actually stored.
        found: crate::objects::ObjectKind,
    },

    /// [`notes_merge_inner`](crate::notes::notes_merge_inner) was asked to merge two notes refs
    /// that both have no notes commit.
    #[error("cannot merge notes: neither '{local}' nor '{remote}' has any notes")]
    MergeBothEmpty {
        /// Local notes ref (merge destination).
        local: String,
        /// Remote notes ref (merge source).
        remote: String,
    },

    /// A manual notes merge was started while an earlier one still has conflict files in the
    /// notes merge worktree.
    #[error("unfinished notes merge: conflict files remain in {}", crate::git_path::user_display_path(.path))]
    MergeInProgress {
        /// The notes merge worktree directory.
        path: std::path::PathBuf,
    },
}

impl NotesError {
    /// Stable snake_case slug for this failure (the former umbrella variant names).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::AlreadyExists { .. } => "note_already_exists",
            Self::InvalidRef { .. } => "invalid_notes_ref",
            Self::UnknownRewriteMode { .. } => "notes_rewrite_mode",
            Self::RefNotCommit { .. } => "notes_ref_not_commit",
            Self::UnexpectedObjectKind { .. } => "notes_object_kind",
            Self::MergeBothEmpty { .. } => "notes_merge_both_empty",
            Self::MergeInProgress { .. } => "notes_merge_in_progress",
        }
    }
}
