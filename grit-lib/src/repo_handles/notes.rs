//! Git notes scoped to a [`Repository`](crate::repo::Repository) and a [`NotesRef`](crate::notes::NotesRef).

use crate::error::Result;
use crate::notes::{
    note_object_name, read_notes_tree, remove_note_entry, upsert_note_entry,
    write_notes_commit_idents, NotesRef,
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
    /// # fn demo() -> grit_lib::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")))?;
    /// let notes = repo.notes().list(&NotesRef::commits())?;
    /// let _ = notes.len();
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self, notes_ref: &NotesRef) -> Result<Vec<NoteListing>> {
        let entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        Ok(entries
            .into_iter()
            .filter_map(|e| {
                let object = note_object_name(&e.path)?.parse().ok()?;
                Some(NoteListing {
                    object,
                    note_blob: e.oid,
                })
            })
            .collect())
    }

    /// Read note bytes for `object`, if present.
    ///
    /// # Errors
    ///
    /// Propagates object read failures.
    pub fn read(&self, notes_ref: &NotesRef, object: &ObjectId) -> Result<Option<Vec<u8>>> {
        let hex = object.to_hex();
        let entries = read_notes_tree(self.repo, notes_ref.as_str())?;
        let Some(entry) = entries
            .iter()
            .find(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()))
        else {
            return Ok(None);
        };
        let blob = self.repo.odb.read(&entry.oid)?;
        Ok(Some(blob.data))
    }

    /// Add or replace the note on `object`.
    ///
    /// # Errors
    ///
    /// Propagates blob write and notes commit failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::notes::NotesRef;
    /// # use grit_lib::objects::ObjectId;
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::repo_handles::NotesWriteOptions;
    /// # fn demo() -> grit_lib::Result<()> {
    /// # let repo = Repository::discover(None)?;
    /// # let object = ObjectId::zero();
    /// let opts = NotesWriteOptions {
    ///     message: "add note".into(),
    ///     committer: "User <u@example.com> 1000000000 +0000".into(),
    ///     author: None,
    /// };
    /// repo.notes().add(&NotesRef::commits(), &object, b"text\n", &opts)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        content: &[u8],
        options: &NotesWriteOptions,
    ) -> Result<()> {
        let blob = self.repo.odb.write(ObjectKind::Blob, content)?;
        let mut entries = read_notes_tree(self.repo, notes_ref.as_str()).unwrap_or_default();
        upsert_note_entry(&mut entries, &object.to_hex(), blob);
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            &entries,
            &options.message,
            options.author.as_deref().unwrap_or(&options.committer),
            &options.committer,
        )
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
        let mut entries = read_notes_tree(self.repo, notes_ref.as_str()).unwrap_or_default();
        if !remove_note_entry(&mut entries, &object.to_hex()) {
            return Ok(());
        }
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            &entries,
            &options.message,
            options.author.as_deref().unwrap_or(&options.committer),
            &options.committer,
        )
    }
}
