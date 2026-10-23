---
title: grit notes
summary: List, show, add, and remove Git notes on commits.
group: History
order: 4
---

## Synopsis

```text
grit notes [--ref REF]
grit notes list [--ref REF]
grit notes show <rev> [--ref REF]
grit notes add <rev> (-m <message> | -F <file|->) [--force] [--ref REF]
grit notes remove <rev>... [--ref REF]
```

## Description

Git **notes** are extra blob objects attached to commits via a notes ref (default `refs/notes/commits`, or `core.notesRef` / `GIT_NOTES_REF`). They do not change history; they add metadata Git and other tools can read with `git notes`.

With no subcommand, `grit notes` lists every note under the selected ref. `show` prints one note body to stdout (like `git notes show`). `add` writes a new note commit; without `--force`, adding when a note already exists fails. `remove` drops notes from the given revisions in a single notes commit when multiple revs are passed.

Notes update commits use your configured `user.name` / `user.email` (and optional `GIT_*` overrides), with the committer date taken from the process clock passed into the library — the same identity rules as [`grit commit`](commit.md).

## Options

| Option | Description |
| --- | --- |
| `--ref` | Notes namespace (`commits`, `refs/notes/commits`, …). Overrides `core.notesRef` / `GIT_NOTES_REF`. |
| `-m`, `--message` | Note text for `add` (conflicts with `--message-file`). |
| `-F`, `--message-file` | Read note text from a file, or `-` for stdin (conflicts with `-m`). |
| `--force` | Replace an existing note on `add`. |

Subcommands: `list`, `show`, `add`, `remove`.

## Examples

```console
$ grit notes add HEAD -m 'Reviewed by Alex'
Added note for abc1234

$ grit notes show HEAD
Reviewed by Alex

$ grit notes
abc1234567890abcdef1234567890abcdef123456  Reviewed by Alex

$ grit notes remove HEAD~1 HEAD
Removed note for def5678

$ grit notes list --ref refs/notes/review
No notes yet.
```

## JSON output

Schema: [v1](/schemas/notes/v1.json) (subcommands also have `notes.list`, `notes.show`, `notes.add`, and `notes.remove` schemas).

Pass `--json` for stable output. The top-level `action` field is `list`, `show`, `add`, or `remove`.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `show`, `add`, or `remove`. |
| `notes_ref` | string | Ref that was read or updated. |
| `notes` | array | For `list`: `object`, `blob`, and `body` for each note. |
| `object` | string | For `show` / `add`: annotated commit id (full hex). |
| `body` | string | For `show`: note text. |
| `replaced` | boolean | For `add`: whether a note was overwritten. |
| `objects` | array | For `remove`: ids whose notes were removed. |

Listing:

```json
{
  "action": "list",
  "notes_ref": "refs/notes/commits",
  "notes": [
    {
      "object": "abc1234567890abcdef1234567890abcdef123456",
      "blob": "d719f2387b35d9751843418fe3b613e61d7fd2d0",
      "body": "Reviewed by Alex\n"
    }
  ]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/notes/v1.json)

List actions render a heading with the notes ref and one bullet per note (short object id and body preview). Show wraps the note body in a fenced `text` block. Add and remove print a short confirmation line per action.

## See also

[Library: Notes](../library/notes/), [`grit show`](show.md)
