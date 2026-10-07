---
title: grit diff
summary: Show uncommitted changes, or the change a commit introduced.
group: Making changes
order: 3
---

## Synopsis

```
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

```
$ grit diff

main.rs
@@ -1 +1 @@
 1    │ - fn main() {}
    1 │ + fn main() {
    2 │ +     println!("hi");
    3 │ + }
```

What did the commit before the last one change?

```
$ grit diff HEAD~1
```

List the files changed by a commit:

```
$ grit diff v0.1 --json --filter '.files[].path'
[
  "README.md",
  "main.rs"
]
```

## JSON output

```
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

| Field | Description |
| --- | --- |
| `files[].status` | `added`, `modified`, `deleted`, `renamed`, `copied`, `type_changed` or `unmerged`. |
| `files[].binary` | `true` for binary files, which have no hunks. |
| `hunks[].lines[].kind` | `context`, `add` or `del`. |
| `hunks[].lines[].old`, `new` | The line's number before and after the change. A line has the numbers that apply to it. |
| `segments` | The line's text in pieces. `emphasis` marks the parts that changed within the line. |

## See also

[grit status](../status/), [grit show](../show/), [grit log](../log/)
