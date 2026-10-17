---
title: Notes
summary: Attach annotated notes to commits via RepositoryNotes and NotesRef.
---

Git notes are auxiliary commits under `refs/notes/*`. The [`notes`](rustdoc:grit_lib::notes) module defines `NotesRef` (for example `NotesRef::commits`), tree helpers, and `resolve_configured_notes_ref` for `GIT_NOTES_REF` / `core.notesRef`. `Repository::notes` exposes `RepositoryNotes` for add (with a `force` flag and `Error::NoteAlreadyExists`), read, `remove_many`, and list with explicit committer/author options (`NotesWriteOptions`).

## Example

<!-- include: grit-examples/src/bin/guide_notes.rs -->

```bash
cargo run --bin guide_notes /path/to/repo
git notes show HEAD
```

The integration test compares the printed note body to `git notes show`.
