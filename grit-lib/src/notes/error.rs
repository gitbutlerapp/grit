//! Typed failures for Git notes read, write, and merge.

use std::path::PathBuf;

use thiserror::Error;

use crate::objects::{ObjectId, ObjectKind};

/// Notes ref configuration, tree layout, and merge failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotesError {
    /// [`RepositoryNotes::add`](crate::repo_handles::RepositoryNotes::add) was called without
    /// `force` when a note already exists on the object.
    #[error("note for object {object} already exists")]
    NoteAlreadyExists {
        /// Annotated object that already has a note.
        object: ObjectId,
    },

    /// A configured or environment notes ref is not under `refs/notes/`.
    #[error("notes ref '{name}' must start with 'refs/notes/'")]
    InvalidNotesRef {
        /// The rejected notes ref name.
        name: String,
    },

    /// Unknown `notes.rewriteMode` or `GIT_NOTES_REWRITE_MODE` value.
    #[error("unknown notes rewrite mode '{mode}'")]
    NotesRewriteMode {
        /// Unrecognized mode string.
        mode: String,
    },

    /// A notes ref resolves to an object that cannot hold notes.
    #[error("notes ref '{notes_ref}' points to a {found}, not a notes commit")]
    NotesRefNotCommit {
        /// Notes ref (or revision) that was read.
        notes_ref: String,
        /// Kind of object the ref resolved to.
        found: ObjectKind,
    },

    /// An object reached while reading a notes tree has the wrong kind.
    #[error("notes object {oid} is a {found}, expected a {expected}")]
    NotesObjectKind {
        /// Object that was read.
        oid: ObjectId,
        /// Kind the notes layout requires at this position.
        expected: ObjectKind,
        /// Kind actually stored.
        found: ObjectKind,
    },

    /// [`notes_merge_inner`](crate::notes::notes_merge_inner) was asked to merge two notes refs
    /// that both have no notes commit.
    #[error("cannot merge notes: neither '{local}' nor '{remote}' has any notes")]
    NotesMergeBothEmpty {
        /// Local notes ref (merge destination).
        local: String,
        /// Remote notes ref (merge source).
        remote: String,
    },

    /// A manual notes merge was started while an earlier one still has conflict files in the
    /// notes merge worktree.
    #[error(
        "unfinished notes merge: conflict files remain in {}",
        crate::git_path::user_display_path(.path.as_path())
    )]
    NotesMergeInProgress {
        /// The notes merge worktree directory.
        path: PathBuf,
    },
}

impl NotesError {
    /// Stable snake_case name for scripts (matches legacy [`crate::error::Error::kind_name`] slugs).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NoteAlreadyExists { .. } => "note_already_exists",
            Self::InvalidNotesRef { .. } => "invalid_notes_ref",
            Self::NotesRewriteMode { .. } => "notes_rewrite_mode",
            Self::NotesRefNotCommit { .. } => "notes_ref_not_commit",
            Self::NotesObjectKind { .. } => "notes_object_kind",
            Self::NotesMergeBothEmpty { .. } => "notes_merge_both_empty",
            Self::NotesMergeInProgress { .. } => "notes_merge_in_progress",
        }
    }
}
