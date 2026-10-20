//! Git notes scoped to a [`Repository`](crate::repo::Repository) and a [`NotesRef`](crate::notes::NotesRef).

use crate::error::Result;
use crate::notes::{
    note_object_name, read_blob_bytes, read_notes_tree, remove_note_entry, upsert_note_entry,
    write_notes_commit_idents, NotesRef, NotesTreeEntry,
};
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;

/// Author/committer lines and commit message for a notes ref update.
#[derive(Debug, Clone)]
pub struct NotesWriteOptions {
    /// First line and body of the notes commit (trailing newline optional).
    pub message: String,
    /// Full committer identity (`Name <email> timestamp tz`).
    pub committer: String,
    /// Full author identity; defaults to `committer` when `None`.
    pub author: Option<String>,
}

/// One `(annotated object, note blob)` pair from a notes tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteListing {
    /// Object the note is attached to.
    pub object: ObjectId,
    /// Blob object id holding note bytes.
    pub note_blob: ObjectId,
}

/// One note with its contents, from [`RepositoryNotes::list_with_bodies`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Object the note is attached to.
    pub object: ObjectId,
    /// Blob object id holding the note bytes.
    pub note_blob: ObjectId,
    /// Note bytes exactly as stored (Git notes are usually UTF-8 text ending in `\n`).
    pub body: Vec<u8>,
}

/// What [`RepositoryNotes::add`] did to the annotated object's note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteAddOutcome {
    /// The object had no note; one was attached.
    Added,
    /// The object already had a note and `force` replaced it.
    Replaced,
}

/// Note entries of a notes tree whose path names an object (other entries are skipped).
fn listings(entries: Vec<NotesTreeEntry>) -> impl Iterator<Item = NoteListing> {
    entries.into_iter().filter_map(|e| {
        let object = note_object_name(&e.path)?.parse().ok()?;
        Some(NoteListing {
            object,
            note_blob: e.oid,
        })
    })
}

/// Notes read/write for one open repository (default ref: [`NotesRef::commits`]).
#[derive(Debug, Copy, Clone)]
pub struct RepositoryNotes<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryNotes<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// List notes under `notes_ref`.
    ///
    /// # Errors
    ///
    /// Propagates notes tree read failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::notes::NotesRef;
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let notes = repo.notes().list(&NotesRef::commits())?;
    /// let _ = notes.len();
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self, notes_ref: &NotesRef) -> Result<Vec<NoteListing>> {
        let entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        Ok(listings(entries).collect())
    }

    /// List notes under `notes_ref` together with their contents, sorted by annotated object.
    ///
    /// # Errors
    ///
    /// - Notes tree read failures (see [`Self::list`]).
    /// - [`Error::NotesObjectKind`](crate::error::Error::NotesObjectKind) when a note entry is
    ///   not a blob, and object read failures (a missing note blob is an error, not an empty
    ///   note).
    pub fn list_with_bodies(&self, notes_ref: &NotesRef) -> Result<Vec<Note>> {
        let entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        let mut notes = Vec::with_capacity(entries.len());
        for listing in listings(entries) {
            notes.push(Note {
                body: read_blob_bytes(self.repo, &listing.note_blob)?,
                object: listing.object,
                note_blob: listing.note_blob,
            });
        }
        notes.sort_by_key(|note| note.object);
        Ok(notes)
    }

    /// Read note bytes for `object`, if present.
    ///
    /// # Errors
    ///
    /// Propagates notes tree and object read failures;
    /// [`Error::NotesObjectKind`](crate::error::Error::NotesObjectKind) when the note entry is
    /// not a blob.
    pub fn read(&self, notes_ref: &NotesRef, object: &ObjectId) -> Result<Option<Vec<u8>>> {
        let hex = object.to_hex();
        let entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        let Some(entry) = entries
            .iter()
            .find(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()))
        else {
            return Ok(None);
        };
        read_blob_bytes(self.repo, &entry.oid).map(Some)
    }

    /// Add or replace the note on `object`; `content` is stored exactly as given.
    ///
    /// When `force` is false and a note already exists, returns
    /// [`Error::NoteAlreadyExists`](crate::error::Error::NoteAlreadyExists).
    ///
    /// # Returns
    ///
    /// Whether the note was newly [`Added`](NoteAddOutcome::Added) or
    /// [`Replaced`](NoteAddOutcome::Replaced).
    ///
    /// # Errors
    ///
    /// Propagates notes tree read errors (invalid existing notes ref), blob write,
    /// and notes commit failures (including a ref store compare-and-swap failure when the
    /// notes ref moved concurrently).
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::notes::NotesRef;
    /// # use grit_lib::objects::ObjectId;
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::repo_handles::NotesWriteOptions;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// # let repo = Repository::discover(None, &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// # let object = ObjectId::zero();
    /// let opts = NotesWriteOptions {
    ///     message: "add note".into(),
    ///     committer: "User <u@example.com> 1000000000 +0000".into(),
    ///     author: None,
    /// };
    /// repo.notes().add(&NotesRef::commits(), &object, b"text\n", &opts, false)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        content: &[u8],
        options: &NotesWriteOptions,
        force: bool,
    ) -> Result<NoteAddOutcome> {
        let hex = object.to_hex();
        let mut entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        let exists = entries
            .iter()
            .any(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()));
        if exists && !force {
            return Err(crate::error::Error::NoteAlreadyExists { object: *object });
        }
        let blob = self.repo.odb().write(ObjectKind::Blob, content)?;
        upsert_note_entry(&mut entries, &hex, blob);
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            &entries,
            &options.message,
            options.author.as_deref().unwrap_or(&options.committer),
            &options.committer,
        )?;
        Ok(if exists {
            NoteAddOutcome::Replaced
        } else {
            NoteAddOutcome::Added
        })
    }

    /// Remove the note on `object` when present.
    ///
    /// # Errors
    ///
    /// Propagates notes commit failures when a note existed.
    pub fn remove(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        options: &NotesWriteOptions,
    ) -> Result<()> {
        self.remove_many(notes_ref, std::slice::from_ref(object), options)?;
        Ok(())
    }

    /// Remove notes on each object in `objects`, writing at most one notes commit.
    ///
    /// Returns the object ids whose notes were removed.
    ///
    /// # Errors
    ///
    /// Propagates notes tree read and commit failures.
    pub fn remove_many(
        &self,
        notes_ref: &NotesRef,
        objects: &[ObjectId],
        options: &NotesWriteOptions,
    ) -> Result<Vec<ObjectId>> {
        let mut entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        let mut removed = Vec::new();
        for object in objects {
            if remove_note_entry(&mut entries, &object.to_hex()) {
                removed.push(*object);
            }
        }
        if removed.is_empty() {
            return Ok(removed);
        }
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            &entries,
            &options.message,
            options.author.as_deref().unwrap_or(&options.committer),
            &options.committer,
        )?;
        Ok(removed)
    }
}
