---
title: Notes
summary: Attach annotated notes to commits via RepositoryNotes and NotesRef.
---

Git notes are auxiliary commits under `refs/notes/*`. The [`notes`](rustdoc:grit_lib::notes) module defines [`NotesRef`](rustdoc:grit_lib::notes::NotesRef) (for example [`NotesRef::commits`](rustdoc:grit_lib::notes::NotesRef), or [`NotesRef::parse`](rustdoc:grit_lib::notes::NotesRef) for a short name like `review`), tree helpers, and [`resolve_configured_notes_ref`](rustdoc:grit_lib::notes::resolve_configured_notes_ref) for `GIT_NOTES_REF` / `core.notesRef`.

[`Repository::notes`](rustdoc:grit_lib::repo::Repository) exposes [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes):

- `add` attaches a note and returns a [`NoteAddOutcome`](rustdoc:grit_lib::repo_handles::NoteAddOutcome) (`Added` or `Replaced`). Without `force`, an existing note is [`Error::NoteAlreadyExists`](rustdoc:grit_lib::error::Error). The content bytes are stored as given.
- `read` returns one note's bytes, `list` returns `(object, note blob)` pairs as [`NoteListing`](rustdoc:grit_lib::repo_handles::NoteListing), and `list_with_bodies` returns every [`Note`](rustdoc:grit_lib::repo_handles::Note) with its contents, sorted by object. A missing or non-blob note object is an error, never an empty note.
- [`remove_many`](rustdoc:grit_lib::repo_handles::RepositoryNotes) removes several notes in one notes commit.

Writes take explicit committer/author lines and a commit message through [`NotesWriteOptions`](rustdoc:grit_lib::repo_handles::NotesWriteOptions). Each write is one notes commit on top of the current tip. The ref moves through the repository's ref store as a compare-and-swap update, so it works the same on loose/packed refs, reftable, or an injected [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore), and a concurrent writer gets an error instead of a lost update. The reflog line (`notes: <message>`) is written in the same transaction whenever the ref already has a reflog or `core.logAllRefUpdates` allows creating one.

A notes ref that points at something other than a notes commit or tree fails with `Error::NotesRefNotCommit`, and a malformed notes tree fails with `Error::NotesObjectKind`.

## Example

<!-- include: grit-examples/src/bin/guide_notes.rs -->

```bash
cargo run --bin guide_notes /path/to/repo
git notes show HEAD
```

The integration test compares the printed note body to `git notes show`.
