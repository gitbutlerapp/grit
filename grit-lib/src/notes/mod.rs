//! Git notes refs and note commit helpers.
//!
//! Library guide: [mod](https://grit-scm.com/docs/library/notes)
//!
//! ```rust
//! use grit_lib::repo::{InitOptions, Repository};
//! # fn main() -> grit_lib::error::Result<()> {
//! let dir = tempfile::tempdir()?;
//! let (_repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
//! let _ = _repo.notes();
//!
//! # Ok(())
//! # }
//! ```
mod core;

pub use core::NotesRewriteMode;
pub use core::NotesRewritePlan;
pub(crate) use core::{
    apply_prepared_notes_rewrite, copy_notes_for_rewrite, lookup_note_blob_oid,
    materialize_notes_tree, notes_merge_inner, prepare_notes_for_rewrite, preview_notes_commit,
    preview_notes_commit_on_tree, read_blob_bytes, read_notes_snapshot, read_notes_tree,
    read_notes_tree_limited, resolve_notes_ref_tree, splice_note_remove, splice_note_upsert,
    write_notes_commit, write_notes_commit_idents, write_notes_commit_idents_on_tree, NotesCommit,
    NotesSnapshot, NotesSpliceResult,
};
pub use core::{
    note_object_name, remove_note_entry, resolve_configured_notes_ref, upsert_note_entry,
    NotesMergeStrategy, NotesRef, NotesRewriteCommand, NotesTreeEntry,
};
