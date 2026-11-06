//! Git notes scoped to a [`Repository`](crate::repo::Repository) and a [`NotesRef`](crate::notes::NotesRef).

use crate::error::Result;
use crate::ident::Signature;
use crate::list_limit::{fetch_plus_one, Limited};
use crate::notes::{
    lookup_note_blob_oid, materialize_notes_tree, note_object_name, preview_notes_commit,
    preview_notes_commit_on_tree, read_blob_bytes, read_notes_snapshot, read_notes_tree_limited,
    remove_note_entry, resolve_notes_ref_tree, splice_note_remove, splice_note_upsert,
    upsert_note_entry, write_notes_commit_idents, write_notes_commit_idents_on_tree, NotesCommit,
    NotesRef, NotesSnapshot, NotesSpliceResult, NotesTreeEntry,
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

/// Preview of [`RepositoryNotes::remove_many`] without writing objects, refs, or reflogs.
///
/// The removals compose: [`Self::notes_ref_tip_after`] is the single notes commit that
/// `remove_many` would write for the same objects and [`NotesWriteOptions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRemoveManyPlan {
    /// Objects whose notes would be removed, in request order. Objects without a note and
    /// repeated objects are left out, matching the return value of `remove_many`.
    pub removed: Vec<ObjectId>,
    /// Notes ref tip before the operation (`None` when the ref did not exist).
    pub notes_ref_tip_before: Option<ObjectId>,
    /// Predicted notes ref tip after removal (`None` when no note would be removed).
    pub notes_ref_tip_after: Option<ObjectId>,
}

/// Notes tree with removals applied in memory, shared by planning and writing.
struct StagedRemoval {
    tip: Option<ObjectId>,
    entries: Vec<NotesTreeEntry>,
    removed: Vec<ObjectId>,
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
        let (_, root_tree) = resolve_notes_ref_tree(self.repo, notes_ref.as_str())?;
        let Some(root_tree) = root_tree else {
            return Ok(None);
        };
        let Some(blob_oid) = lookup_note_blob_oid(self.repo, &root_tree, &hex)? else {
            return Ok(None);
        };
        read_blob_bytes(self.repo, &blob_oid).map(Some)
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
        let (tip, root_tree, outcome) = self.read_for_add(notes_ref, object, force)?;
        let _overlay = MemOverlayGuard::enter(self.repo.odb());
        let blob = self.repo.odb().write(ObjectKind::Blob, content)?;
        let notes_ref_tip_after =
            self.preview_add_commit(notes_ref, tip, root_tree, object, blob, options)?;
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
    /// Same as [`Self::plan_remove_many`].
    pub fn plan_remove(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        options: &NotesWriteOptions,
    ) -> Result<NoteRemovePlan> {
        let plan = self.plan_remove_many(notes_ref, std::slice::from_ref(object), options)?;
        Ok(NoteRemovePlan {
            object: *object,
            had_note: !plan.removed.is_empty(),
            notes_ref_tip_before: plan.notes_ref_tip_before,
            notes_ref_tip_after: plan.notes_ref_tip_after,
        })
    }

    /// Preview [`Self::remove_many`] without persisting trees, commits, or refs.
    ///
    /// All removals are applied to one notes tree, so the predicted tip is the single notes
    /// commit `remove_many` would write. Object writes are confined to the ODB in-memory
    /// overlay and discarded before return.
    ///
    /// # Errors
    ///
    /// Propagates notes tree read failures and invalid identities in `options`.
    pub fn plan_remove_many(
        &self,
        notes_ref: &NotesRef,
        objects: &[ObjectId],
        options: &NotesWriteOptions,
    ) -> Result<NoteRemoveManyPlan> {
        let StagedRemoval {
            tip,
            entries,
            removed,
        } = self.stage_removals(notes_ref, objects)?;
        let notes_ref_tip_after = if removed.is_empty() {
            None
        } else {
            let _overlay = MemOverlayGuard::enter(self.repo.odb());
            Some(self.preview_commit(tip, &entries, options)?)
        };
        Ok(NoteRemoveManyPlan {
            removed,
            notes_ref_tip_before: tip,
            notes_ref_tip_after,
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
        let (tip, root_tree, outcome) = self.read_for_add(notes_ref, object, force)?;
        let blob = self.repo.odb().write(ObjectKind::Blob, content)?;
        self.write_add_commit(notes_ref, tip, root_tree, object, blob, options)?;
        Ok(outcome)
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
        let (tip, root_tree) = resolve_notes_ref_tree(self.repo, notes_ref.as_str())?;
        let Some(mut root_tree) = root_tree else {
            return Ok(Vec::new());
        };
        let mut removed = Vec::new();
        for object in objects {
            let hex = object.to_hex();
            let had = lookup_note_blob_oid(self.repo, &root_tree, &hex)?.is_some();
            if !had {
                continue;
            }
            match splice_note_remove(self.repo, root_tree, &hex)? {
                NotesSpliceResult::Tree(new_root) => {
                    root_tree = new_root;
                    removed.push(*object);
                }
                NotesSpliceResult::Fallback => {
                    let StagedRemoval {
                        tip: staged_tip,
                        entries,
                        removed: staged_removed,
                    } = self.stage_removals(notes_ref, objects)?;
                    if !staged_removed.is_empty() {
                        self.write_commit(notes_ref, staged_tip, &entries, options)?;
                    }
                    return Ok(staged_removed);
                }
            }
        }
        if !removed.is_empty() {
            let (author, committer) = options.header_lines()?;
            write_notes_commit_idents_on_tree(
                self.repo,
                notes_ref.as_str(),
                tip,
                root_tree,
                &options.message,
                &author,
                &committer,
            )?;
        }
        Ok(removed)
    }

    /// Read `notes_ref` and classify an add on `object`, refusing to replace without `force`.
    fn read_for_add(
        &self,
        notes_ref: &NotesRef,
        object: &ObjectId,
        force: bool,
    ) -> Result<(Option<ObjectId>, Option<ObjectId>, NoteAddOutcome)> {
        let (tip, root_tree) = resolve_notes_ref_tree(self.repo, notes_ref.as_str())?;
        let hex = object.to_hex();
        let exists = match root_tree {
            Some(tree) => lookup_note_blob_oid(self.repo, &tree, &hex)?.is_some(),
            None => false,
        };
        if exists && !force {
            return Err(crate::error::Error::NoteAlreadyExists { object: *object });
        }
        let outcome = if exists {
            NoteAddOutcome::Replaced
        } else {
            NoteAddOutcome::Added
        };
        Ok((tip, root_tree, outcome))
    }

    /// Read `notes_ref` and drop the note of each object in `objects` from the in-memory tree.
    fn stage_removals(&self, notes_ref: &NotesRef, objects: &[ObjectId]) -> Result<StagedRemoval> {
        let NotesSnapshot { tip, mut entries } =
            read_notes_snapshot(self.repo, notes_ref.as_str())?;
        let removed = objects
            .iter()
            .filter(|object| remove_note_entry(&mut entries, &object.to_hex()))
            .copied()
            .collect();
        Ok(StagedRemoval {
            tip,
            entries,
            removed,
        })
    }

    fn preview_add_commit(
        &self,
        notes_ref: &NotesRef,
        tip: Option<ObjectId>,
        root_tree: Option<ObjectId>,
        object: &ObjectId,
        blob: ObjectId,
        options: &NotesWriteOptions,
    ) -> Result<ObjectId> {
        let tree = self.materialize_add_tree(notes_ref, root_tree, object, blob)?;
        let (author, committer) = options.header_lines()?;
        let commit = NotesCommit {
            message: &options.message,
            author: &author,
            committer: &committer,
        };
        preview_notes_commit_on_tree(self.repo, tip, tree, &commit)
    }

    fn write_add_commit(
        &self,
        notes_ref: &NotesRef,
        tip: Option<ObjectId>,
        root_tree: Option<ObjectId>,
        object: &ObjectId,
        blob: ObjectId,
        options: &NotesWriteOptions,
    ) -> Result<()> {
        let tree = self.materialize_add_tree(notes_ref, root_tree, object, blob)?;
        let (author, committer) = options.header_lines()?;
        write_notes_commit_idents_on_tree(
            self.repo,
            notes_ref.as_str(),
            tip,
            tree,
            &options.message,
            &author,
            &committer,
        )
    }

    fn materialize_add_tree(
        &self,
        notes_ref: &NotesRef,
        root_tree: Option<ObjectId>,
        object: &ObjectId,
        blob: ObjectId,
    ) -> Result<ObjectId> {
        let hex = object.to_hex();
        match splice_note_upsert(self.repo, root_tree, &hex, blob)? {
            NotesSpliceResult::Tree(tree) => Ok(tree),
            NotesSpliceResult::Fallback => {
                let NotesSnapshot { mut entries, .. } =
                    read_notes_snapshot(self.repo, notes_ref.as_str())?;
                upsert_note_entry(&mut entries, &hex, blob);
                materialize_notes_tree(self.repo, &entries)
            }
        }
    }

    /// Object id of the notes commit [`Self::write_commit`] would create; callers confine the
    /// object writes with a [`MemOverlayGuard`].
    fn preview_commit(
        &self,
        tip: Option<ObjectId>,
        entries: &[NotesTreeEntry],
        options: &NotesWriteOptions,
    ) -> Result<ObjectId> {
        let (author, committer) = options.header_lines()?;
        let commit = NotesCommit {
            message: &options.message,
            author: &author,
            committer: &committer,
        };
        preview_notes_commit(self.repo, tip, entries, &commit)
    }

    /// Write `entries` as a notes commit on `tip` and move `notes_ref` from `tip` to it.
    fn write_commit(
        &self,
        notes_ref: &NotesRef,
        tip: Option<ObjectId>,
        entries: &[NotesTreeEntry],
        options: &NotesWriteOptions,
    ) -> Result<()> {
        let (author, committer) = options.header_lines()?;
        write_notes_commit_idents(
            self.repo,
            notes_ref.as_str(),
            tip,
            entries,
            &options.message,
            &author,
            &committer,
        )
    }
}
