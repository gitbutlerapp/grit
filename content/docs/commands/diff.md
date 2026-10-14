---
title: grit diff
summary: Show uncommitted, staged, commit, or range changes.
group: Making changes
order: 3
---

## Synopsis

```text
grit diff [--staged] [--unstaged] [--against <ref>] [<commit> [<commit>]]
grit diff [--staged] [--unstaged] [--against <ref>] [<base>..<tip>]
```

## Description

With no argument, shows every uncommitted change: the working tree and index together, compared with the last commit (`HEAD`). Pass `--staged` to compare only the index to `HEAD` (staged changes). Pass `--unstaged` to compare only the working tree to the index (changes not yet staged). Pass `--against <ref>` to compare the working tree to another ref’s tree instead of `HEAD` (useful for “what changed since `main`?” while keeping a single commit argument as “show this commit’s patch”).

With one commit, shows the change that commit introduced (its first parent’s tree against its own tree). Commit, range, **staged**, and **unstaged** diffs detect renames by default (Git `-M50`): a move with small edits appears as one `renamed` file instead of a full delete and add. With two commits, or a two-dot range such as `A..B`, shows the tree diff between those commits with the same rename detection. A three-dot range `A...B` diffs from the merge base of `A` and `B` to `B`.

Each file starts with its path (UTF-8 names print literally; C-quoted only when the name contains control characters, unless `core.quotepath=true`), and each hunk with its position. Lines have two number columns, the line number before and after the change. In a terminal, removed lines are red, added lines are green, and the words that changed within a line are highlighted. When the output isn't a terminal, or `NO_COLOR` is set, lines are marked with `-` and `+` instead. Three lines of unchanged context are shown around each change. Binary files print `Binary file differs` when the blob changed (mode-only binary changes show mode lines only).

`<commit>` can be anything that names a commit: a full or short commit id, a branch or tag name, or an expression like `HEAD~2`.

## Options

| Option | Description |
| --- | --- |
| `--staged`, `-S` | Staged changes only (index vs `HEAD`). Cannot be combined with `--unstaged`, commit arguments, or `--against`. |
| `--unstaged` | Unstaged changes only (worktree vs index). Cannot be combined with `--staged`, commit arguments, or `--against`. |
| `--against <ref>` | Compare the worktree to `<ref>`’s tree instead of `HEAD`. No commit arguments. |
| `<commit>` | One commit (that commit’s patch), two commits (tree diff), or a range `A..B` / `A...B`. Omit for uncommitted changes. |

## Examples

What have I changed since the last commit?

```console
$ grit diff

main.rs
@@ -1 +1 @@
 1    │ - fn main() {}
    1 │ + fn main() {
    2 │ +     println!("hi");
    3 │ + }
```

What is staged but not yet committed?

```console
$ grit diff --staged
```

What changed in the working tree since the last `grit add`?

```console
$ grit diff --unstaged
```

What changed between two commits?

```console
$ grit diff HEAD~1 HEAD
$ grit diff HEAD~1..HEAD
```

What does my worktree look like compared to `main`?

```console
$ grit diff --against main
```

What did the commit before the last one change?

```console
$ grit diff HEAD~1
```

List the files changed by a commit:

```console
$ grit diff v0.1 --json --filter '.files[].path'
[
  "README.md",
  "main.rs"
]
```

## JSON output

Schema: [v1](/schemas/diff/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `files` | array | Changed files, each with `path`, `status`, `binary`, optional `old_mode` / `new_mode`, optional `old_encoding_lossy` / `new_encoding_lossy`, and `hunks`. |

Nested fields include `files[].status` (`added`, `modified`, `deleted`, `renamed`, and so on), optional `files[].old_path` when `status` is `renamed`, `hunks[].old_start` / `new_start` / `old_lines` / `new_lines`, optional `hunks[].context` (enclosing definition line), `hunks[].lines[].kind` (`context`, `add`, or `del`), line numbers on `old` and `new`, `segments` with optional `emphasis` on intra-line changes, and `no_newline_at_eof` when that side lacks a trailing newline (Git’s `\ No newline at end of file`). For unstaged and other worktree-sourced sides, line text is normalized to repository form (`.gitattributes` / `core.autocrlf`) before diffing, so trailing `\r` from CRLF checkouts does not appear in `segments[].text`. Commit and staged diffs compare stored blobs and may still show carriage returns when the on-disk object bytes differ. When file bytes are not valid UTF-8, `old_encoding_lossy` or `new_encoding_lossy` is true and non-UTF-8 bytes appear as U+FFFD in text fields.

Example:

```json
{
  "files": [
    {
      "path": "main.rs",
      "status": "modified",
      "binary": false,
      "hunks": [
        {
          "old_start": 1,
          "new_start": 1,
          "lines": [
            {
              "kind": "del",
              "old": 1,
              "segments": [
                { "text": "fn main() ", "emphasis": false },
                { "text": "{}", "emphasis": true }
              ]
            },
            {
              "kind": "add",
              "new": 1,
              "segments": [
                { "text": "fn main() ", "emphasis": false },
                { "text": "{", "emphasis": true }
              ]
            }
          ]
        }
      ]
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/diff/v1.json)
Pass [`--markdown`](../global-options/) for one `diff`-fenced unified patch per changed file (binary files note `Binary file differs.` when content changed):

```text
File: main.rs

@@ -1,1 +1,2 @@
 fn main() {
+    println!("hello");
 }
```

Each changed file is introduced with a level-2 heading on stdout, followed by a `diff`-fenced patch.

## See also

[grit status](../status/), [grit show](../show/), [grit log](../log/)
