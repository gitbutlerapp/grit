---
title: Notes
summary: Attach annotated notes to commits via RepositoryNotes and NotesRef.
---

Git notes are auxiliary commits under `refs/notes/*`. The [`notes`](rustdoc:grit_lib::notes) module defines [`NotesRef`](rustdoc:grit_lib::notes::NotesRef) (for example [`NotesRef::commits`](rustdoc:grit_lib::notes::NotesRef), or [`NotesRef::parse`](rustdoc:grit_lib::notes::NotesRef) for a short name like `review`), tree helpers, and [`resolve_configured_notes_ref`](rustdoc:grit_lib::notes::resolve_configured_notes_ref) for `GIT_NOTES_REF` / `core.notesRef`.

[`Repository::notes`](rustdoc:grit_lib::repo::Repository) exposes [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes):

- `add` attaches a note and returns a [`NoteAddOutcome`](rustdoc:grit_lib::repo_handles::NoteAddOutcome) (`Added` or `Replaced`). Without `force`, an existing note is [`Error::NoteAlreadyExists`](rustdoc:grit_lib::error::Error). The content bytes are stored as given.
- `read` returns one note's bytes, `list` returns `(object, note blob)` pairs as [`NoteListing`](rustdoc:grit_lib::repo_handles::NoteListing), and `list_with_bodies` returns every [`Note`](rustdoc:grit_lib::repo_handles::Note) with its contents, sorted by object. A missing or non-blob note object is an error, never an empty note.
- [`remove_many`](rustdoc:grit_lib::repo_handles::RepositoryNotes) removes several notes in one notes commit.
- `plan_add` and `plan_remove_many` preview a write without touching objects, refs, or reflogs. [`NoteAddPlan`](rustdoc:grit_lib::repo_handles::NoteAddPlan) and [`NoteRemoveManyPlan`](rustdoc:grit_lib::repo_handles::NoteRemoveManyPlan) report the notes ref tip before the write and the notes commit the write would create. A batch plan applies every removal to one tree, so its predicted tip is the commit `remove_many` writes for the same objects and options.

Writes take an explicit committer and optional author ([`Signature`](rustdoc:grit_lib::ident::Signature) values, usually from `repo.identity`) and a commit message through [`NotesWriteOptions`](rustdoc:grit_lib::repo_handles::NotesWriteOptions). Each write is one notes commit on top of the tip the existing notes were read from. The ref moves through the repository's ref store as a compare-and-swap update from that same tip, so it works the same on loose/packed refs, reftable, or an injected [`MemoryRefStore`](rustdoc:grit_lib::refs::store::MemoryRefStore), and an `add` or `remove_many` that races another notes writer fails instead of dropping the other writer's note. The lower-level [`write_notes_commit`](rustdoc:grit_lib::notes::write_notes_commit) replaces the whole tree with the entries you pass, so callers that read entries themselves own that race. The reflog line (`notes: <message>`) is written in the same transaction whenever the ref already has a reflog or `core.logAllRefUpdates` allows creating one.

A notes ref that points at something other than a notes commit or tree fails with `Error::NotesRefNotCommit`, and a malformed notes tree fails with `Error::NotesObjectKind`.

When `notes.rewriteRef` (or `GIT_NOTES_REWRITE_REF`) lists one or more refs under `refs/notes/`, grit copies notes from old commits to their replacements after a rebase or other history rewrite, and after amend when post-rewrite hooks run. Enable or disable each command with `notes.rewrite.rebase` and `notes.rewrite.amend` (both default to true). Combine colliding notes with `notes.rewriteMode` (`concatenate`, `cat_sort_uniq`, `overwrite`, or `ignore`; default `concatenate`). The library entry point is [`copy_notes_for_rewrite`](rustdoc:grit_lib::notes::copy_notes_for_rewrite).

## Example

<!-- include: grit-examples/src/bin/guide_notes.rs -->

```bash
cargo run --bin guide_notes /path/to/repo
git notes show HEAD
```

The integration test compares the printed note body to `git notes show`.
