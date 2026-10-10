---
title: grit diff
summary: Show uncommitted changes, or the change a commit introduced.
group: Making changes
order: 3
---

## Synopsis

```text
grit diff [<commit>]
```

## Description

With no argument, shows every uncommitted change: the working tree, staged or not, compared with the last commit. With a commit, shows the change that commit introduced, compared with its first parent.

Each file starts with its path, and each hunk with its position. Lines have two number columns, the line number before and after the change. In a terminal, removed lines are red, added lines are green, and the words that changed within a line are highlighted. When the output isn't a terminal, or `NO_COLOR` is set, lines are marked with `-` and `+` instead. Three lines of unchanged context are shown around each change. Binary files are reported but not shown.

`<commit>` can be anything that names a commit: a full or short commit id, a branch or tag name, or an expression like `HEAD~2`.

## Options

| Option | Description |
| --- | --- |
| `<commit>` | The commit whose change to show. Omit to show uncommitted changes. |

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

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `files` | array | Changed files, each with `path`, `status`, `binary`, optional `old_mode` / `new_mode`, optional `old_encoding_lossy` / `new_encoding_lossy`, and `hunks`. |

Nested fields include `files[].status` (`added`, `modified`, `deleted`, and so on), `hunks[].old_start` / `new_start` / `old_lines` / `new_lines`, optional `hunks[].context` (enclosing definition line), `hunks[].lines[].kind` (`context`, `add`, or `del`), line numbers on `old` and `new`, `segments` with optional `emphasis` on intra-line changes, and `no_newline_at_eof` when that side lacks a trailing newline (Git’s `\ No newline at end of file`). Carriage returns inside a line are preserved in `segments[].text` (for example CRLF→LF). When file bytes are not valid UTF-8, `old_encoding_lossy` or `new_encoding_lossy` is true and non-UTF-8 bytes appear as U+FFFD in text fields.

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

Pass [`--markdown`](../global-options/) for one `diff`-fenced unified patch per changed file:

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
