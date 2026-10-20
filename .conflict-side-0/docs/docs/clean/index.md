# grit clean

> Remove untracked files from the working tree.

## Synopsis

```text
grit clean [<path>...]
grit clean -f [<path>...]
grit clean -f --ignored [<path>...]
```

## Description

Removes untracked files (and untracked directories) from the working tree. Tracked files and nested Git repositories (directories that contain their own `.git`) are never removed.

Without `-f` / `--force`, `grit clean` is a **dry run**: it prints what would be removed and leaves the work tree unchanged. This differs from Git’s `clean.requireForce` error on purpose — previewing is the default so you can see the impact before deleting anything.

Pass `-f` to actually delete the listed paths. Pass `--ignored` to also remove ignored files (similar to `git clean -x`).

With path arguments, only untracked paths matching those pathspecs are considered.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Limit cleaning to these paths. Omit to scan the whole tree. |
| `-f`, `--force` | Delete untracked paths instead of only listing them. |
| `--ignored` | Remove ignored files and directories as well. |

## Examples

Preview removals (default):

```console
$ grit clean
Would remove untracked.txt
Would remove nested/
```

Delete untracked paths:

```console
$ grit clean -f
Removed untracked.txt
Removed nested/
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `removed` | array of strings | Repository-relative paths removed, or that would be removed when `-f` was not passed. |

Example (dry run):

```json
{
  "removed": [
    "untracked.txt",
    "nested/"
  ]
}
```

## Markdown output

Pass `--markdown` (see [Global options](https://grit-scm.com/docs/global-options/index.md)) for a bullet list of paths under **Would remove** or **Removed**.

## See also

[grit status](https://grit-scm.com/docs/status/index.md), [grit add](https://grit-scm.com/docs/add/index.md)
