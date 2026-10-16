---
title: Notes
summary: Attach annotated notes to commits via RepositoryNotes and NotesRef.
---

Git notes are auxiliary commits under `refs/notes/*`. The [`notes`](rustdoc:grit_lib::notes) module defines [`NotesRef`](rustdoc:grit_lib::notes::NotesRef) (for example [`NotesRef::commits`](rustdoc:grit_lib::notes::NotesRef)), tree helpers, and [`resolve_configured_notes_ref`](rustdoc:grit_lib::notes::resolve_configured_notes_ref) for `GIT_NOTES_REF` / `core.notesRef`. [`Repository::notes`](rustdoc:grit_lib::repo::Repository) exposes [`RepositoryNotes`](rustdoc:grit_lib::repo_handles::RepositoryNotes) for add (with a `force` flag and [`Error::NoteAlreadyExists`](rustdoc:grit_lib::error::Error)), read, [`remove_many`](rustdoc:grit_lib::repo_handles::RepositoryNotes), and list with explicit committer/author options ([`NotesWriteOptions`](rustdoc:grit_lib::repo_handles::NotesWriteOptions)).

## Example

<!-- include: grit-examples/src/bin/guide_notes.rs -->

```bash
cargo run --bin guide_notes /path/to/repo
git notes show HEAD
```

The integration test compares the printed note body to `git notes show`.
