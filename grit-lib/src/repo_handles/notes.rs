//! Git notes scoped to a [`Repository`](crate::repo::Repository) and a [`NotesRef`](crate::notes::NotesRef).

use crate::error::Result;
use crate::ident::Signature;
use crate::list_limit::{fetch_plus_one, Limited};
use crate::notes::{
    note_object_name, preview_notes_commit, read_blob_bytes, read_notes_snapshot, read_notes_tree,
    read_notes_tree_limited, remove_note_entry, upsert_note_entry, write_notes_commit_idents,
    NotesCommit, NotesRef, NotesSnapshot, NotesTreeEntry,
};
use crate::objects::{ObjectId, ObjectKind};
use crate::porcelain::merge::MemOverlayGuard;
use crate::repo::Repository;

/// Author, committer, and commit message for a notes ref update.
#[derive(Debug, Clone)]
pub struct NotesWriteOptions {
    /// First line and body of the notes commit (trailing newline optional).
    pub message: String,
    /// Committer of the notes commit (for example from [`Repository::identity`]).
    pub committer: Signature,
    /// Author of the notes commit; defaults to [`Self::committer`] when `None`.
    pub author: Option<Signature>,
}

impl NotesWriteOptions {
    /// Validated `(author, committer)` header lines for the notes commit.
    fn header_lines(&self) -> Result<(String, String)> {
        let committer = &self.committer;
        let author = self.author.as_ref().unwrap_or(committer);
        for signature in [author, committer] {
            signature.validate()?;
        }
        Ok((author.header_line(), committer.header_line()))
    }
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

/// Preview of [`RepositoryNotes::add`] without writing objects, refs, or reflogs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteAddPlan {
    /// Annotated object the note would attach to.
    pub object: ObjectId,
    /// Whether the note would be newly added or would replace an existing note.
    pub outcome: NoteAddOutcome,
    /// Notes ref tip before the operation (`None` when the ref did not exist).
    pub notes_ref_tip_before: Option<ObjectId>,
    /// Predicted notes ref tip after the operation.
    pub notes_ref_tip_after: ObjectId,
    /// Blob id that would store the note bytes.
    pub note_blob: ObjectId,
}

/// Preview of removing a note on one object without writing objects, refs, or reflogs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRemovePlan {
    /// Annotated object whose note would be removed.
    pub object: ObjectId,
    /// Whether a note exists on `object` today.
    pub had_note: bool,
    /// Notes ref tip before the operation (`None` when the ref did not exist).
    pub notes_ref_tip_before: Option<ObjectId>,
    /// Predicted notes ref tip after removal (`None` when nothing would change).
    pub notes_ref_tip_after: Option<ObjectId>,
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
    /// let notes = repo.notes().list(&NotesRef::commits(), None)?;
    /// let _ = notes.items.len();
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self, notes_ref: &NotesRef, limit: Option<usize>) -> Result<Limited<NoteListing>> {
        let cap = fetch_plus_one(limit);
        let (entries, stopped_early) = read_notes_tree_limited(self.repo, notes_ref.as_str(), cap)?;
        let notes: Vec<NoteListing> = listings(entries).collect();
        let mut limited = Limited::from_vec(notes, limit);
        if stopped_early {
            limited.truncated = true;
        }
        Ok(limited)
    }

    /// List notes under `notes_ref` together with their contents, sorted by annotated object.
    ///
    /// # Errors
    ///
    /// - Notes tree read failures (see [`Self::list`]).
    /// - [`Error::NotesObjectKind`](crate::error::Error::NotesObjectKind) when a note entry is
    ///   not a blob, and object read failures (a missing note blob is an error, not an empty
    ///   note).
    pub fn list_with_bodies(
        &self,
        notes_ref: &NotesRef,
        limit: Option<usize>,
    ) -> Result<Limited<Note>> {
        let listed = self.list(notes_ref, limit)?;
        let mut notes = Vec::with_capacity(listed.items.len());
        for listing in listed.items {
            notes.push(Note {
                body: read_blob_bytes(self.repo, &listing.note_blob)?,
                object: listing.object,
                note_blob: listing.note_blob,
            });
        }
        notes.sort_by_key(|note| note.object);
        Ok(Limited {
            items: notes,
            truncated: listed.truncated,
        })
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

    /// Preview [`Self::add`] without persisting blobs, trees, commits, or refs.
    ///
    /// Object writes are confined to the ODB in-memory overlay and discarded before return.
    ///
    /// # Errors
    ///
    /// Same as [`Self::add`], including [`Error::NoteAlreadyExists`](crate::error::Error::NoteAlreadyExists)
    /// when `force` is false and a note already exists.
    pub fn plan_add(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        content: &[u8],
        options: &NotesWriteOptions,
        force: bool,
    ) -> Result<NoteAddPlan> {
        let hex = object.to_hex();
        let NotesSnapshot { tip, mut entries } =
            read_notes_snapshot(self.repo, notes_ref.as_str())?;
        let exists = entries
            .iter()
            .any(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()));
        if exists && !force {
            return Err(crate::error::Error::NoteAlreadyExists { object: *object });
        }
        let outcome = if exists {
            NoteAddOutcome::Replaced
        } else {
            NoteAddOutcome::Added
        };
        let _overlay = MemOverlayGuard::enter(self.repo.odb());
        let blob = self.repo.odb().write(ObjectKind::Blob, content)?;
        upsert_note_entry(&mut entries, &hex, blob);
        let (author, committer) = options.header_lines()?;
        let commit = NotesCommit {
            message: &options.message,
            author: &author,
            committer: &committer,
        };
        let notes_ref_tip_after = preview_notes_commit(self.repo, tip, &entries, &commit)?;
        Ok(NoteAddPlan {
            object: *object,
            outcome,
            notes_ref_tip_before: tip,
            notes_ref_tip_after,
            note_blob: blob,
        })
    }

    /// Preview [`Self::remove`] on `object` without persisting blobs, trees, commits, or refs.
    ///
    /// # Errors
    ///
    /// Propagates notes tree read failures.
    pub fn plan_remove(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        options: &NotesWriteOptions,
    ) -> Result<NoteRemovePlan> {
        let NotesSnapshot { tip, mut entries } =
            read_notes_snapshot(self.repo, notes_ref.as_str())?;
        let had_note = remove_note_entry(&mut entries, &object.to_hex());
        if !had_note {
            return Ok(NoteRemovePlan {
                object: *object,
                had_note: false,
                notes_ref_tip_before: tip,
                notes_ref_tip_after: None,
            });
        }
        let _overlay = MemOverlayGuard::enter(self.repo.odb());
        let (author, committer) = options.header_lines()?;
        let commit = NotesCommit {
            message: &options.message,
            author: &author,
            committer: &committer,
        };
        let notes_ref_tip_after = preview_notes_commit(self.repo, tip, &entries, &commit)?;
        Ok(NoteRemovePlan {
            object: *object,
            had_note: true,
            notes_ref_tip_before: tip,
            notes_ref_tip_after: Some(notes_ref_tip_after),
        })
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
    /// and notes commit failures. The notes ref is updated only if it still points at the tip
    /// the existing notes were read from, so a concurrent notes update fails this call instead
    /// of being dropped.
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
    ///     committer: "User <u@example.com> 1000000000 +0000".parse()?,
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
        let NotesSnapshot { tip, mut entries } =
            read_notes_snapshot(self.repo, notes_ref.as_str())?;
        let exists = entries
            .iter()
            .any(|e| note_object_name(&e.path).as_deref() == Some(hex.as_str()));
        if exists && !force {
            return Err(crate::error::Error::NoteAlreadyExists { object: *object });
        }
        let blob = self.repo.odb().write(ObjectKind::Blob, content)?;
        upsert_note_entry(&mut entries, &hex, blob);
        let (author, committer) = options.header_lines()?;
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            tip,
            &entries,
            &options.message,
            &author,
            &committer,
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
    /// Propagates notes tree read and commit failures, including the compare-and-swap failure
    /// described on [`Self::add`] when the notes ref moves concurrently.
    pub fn remove_many(
        &self,
        notes_ref: &NotesRef,
        objects: &[ObjectId],
        options: &NotesWriteOptions,
    ) -> Result<Vec<ObjectId>> {
        let NotesSnapshot { tip, mut entries } =
            read_notes_snapshot(self.repo, notes_ref.as_str())?;
        let mut removed = Vec::new();
        for object in objects {
            if remove_note_entry(&mut entries, &object.to_hex()) {
                removed.push(*object);
            }
        }
        if removed.is_empty() {
            return Ok(removed);
        }
        let (author, committer) = options.header_lines()?;
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            tip,
            &entries,
            &options.message,
            &author,
            &committer,
        )?;
        Ok(removed)
    }
}
