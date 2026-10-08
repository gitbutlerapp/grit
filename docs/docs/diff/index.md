# grit diff

> Show uncommitted changes, or the change a commit introduced.

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
| `files` | array | Changed files, each with `path`, `status`, `binary`, and `hunks`. |

Nested fields include `files[].status` (`added`, `modified`, `deleted`, and so on), `hunks[].lines[].kind` (`context`, `add`, or `del`), line numbers on `old` and `new`, and `segments` with optional `emphasis` on intra-line changes.

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

## See also

[grit status](https://grit-scm.com/docs/status/index.md), [grit show](https://grit-scm.com/docs/show/index.md), [grit log](https://grit-scm.com/docs/log/index.md)
