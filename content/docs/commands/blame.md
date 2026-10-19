---
title: grit blame
summary: Show who last modified each line of a file.
group: History
order: 2
---

## Synopsis

```text
grit blame [--rev <commit>] [-L START,END] [--first-parent] <file>
```

## Description

Walks history for one file and prints each line with the commit that last modified it, the author, when that commit was made, and the line number in the blamed revision. The file operand uses Git pathspec cwd rules (same as [`grit diff`](../diff/)): from a subdirectory, a bare name like `f.txt` refers to that path under your current directory.

Blame reuses parsed blob lines, tree path lookups, and parent/child line maps across the history walk so large files with deep history stay responsive. On this repository, blaming `grit-lib/src/pack.rs` (~4.6k lines, ~90 commits) completes in a few seconds on a release build; system `git blame` on the same path is still faster, and very large histories may take longer than `git`.

When a commit renamed or moved the file (including rename-and-edit in the same commit), blame follows the predecessor path using rename detection with Git's default similarity threshold (50%, like `git blame` with `-M`). This behavior is independent of `diff.renames` (system Git blame still follows renames when that diff setting is off). Git's `diff.renameLimit` and `status.renameLimit` cap how many rename pairings diff/status consider; they do not change blame's similarity percentage.

## Options

| Option | Description |
| --- | --- |
| `--rev <commit>` | Blame the file as it existed at this revision (branch, tag, oid, `HEAD~N`, …). Defaults to `HEAD`. |
| `-L START,END` | Limit output to an inclusive 1-based line range in the final file. Blame stops once every line in the range is attributed. |
| `--lines` | Same as `-L` (long form). |
| `--first-parent` | At merge commits, follow only the first parent when attributing lines. Default matches Git blame: all parents are considered. |

## Examples

Blame a file at `HEAD`:

```console
$ grit blame src/lib.rs
a1b2c3d (ada 2 days ago    1) use std::io;
f4e5d6c (bob 1 week ago    2) fn main() {
```

Blame an older revision and a slice of lines:

```console
$ grit blame --rev=v1.0 -L 10,25 README.md
```

Scripting with JSON:

```console
$ grit blame --json story.txt
```

## JSON output

Schema: [v1](/schemas/blame/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `file` | string | Work-tree path that was blamed (repository-relative after cwd resolution). |
| `rev` | string | Full commit oid of the start revision. |
| `lines` | array | One record per line (after any `-L` filter), in final-line order. |
| `lines[].commit` | string | Full commit oid for the line. |
| `lines[].original_line` | number | 1-based line number in the originating commit. |
| `lines[].final_line` | number | 1-based line number in the blamed revision. |
| `lines[].content` | string | Line text. |
| `commits` | object | Map of commit oid → metadata for every line. |
| `commits.<oid>.author` | object | Author with `name`, `email`, and `date` (RFC 3339 — same shape as [`grit show`](../show/)). |
| `commits.<oid>.summary` | string | First line of the commit message. |

Example:

```json
{
  "file": "story.txt",
  "rev": "92501f188ae0815af09a0cef6e121eddda4113cf",
  "lines": [
    {
      "commit": "919c45f33de5e5c0bd05f8ffb089f697f4644976",
      "original_line": 1,
      "final_line": 1,
      "content": "alpha ONE"
    }
  ],
  "commits": {
    "919c45f33de5e5c0bd05f8ffb089f697f4644976": {
      "author": {
        "name": "Blame Test",
        "email": "blame@example.com",
        "date": "2024-03-09T12:26:40Z"
      },
      "summary": "edit first line"
    }
  }
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/blame/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

- [`grit log`](log.md) — commit history
- [`grit show`](show.md) — one commit's message and file stats
