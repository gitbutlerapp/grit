---
title: grit clean
summary: Remove untracked files from the working tree.
group: Maintenance
order: 1
---

## Synopsis

```text
grit clean [--pathspec-from-file <file|->] [--pathspec-file-nul] [<path>...]
grit clean -f [--pathspec-from-file <file|->] [<path>...]
grit clean [--dry-run] [--ignored | --only-ignored] [-e <pattern>] [--files-only] [<path>...]
```

## Description

Removes untracked files (and untracked directories) from the working tree. Tracked files and nested Git repositories (directories that contain their own `.git`) are never removed.

Without `-f` / `--force`, `grit clean` is a **dry run**: it prints what would be removed and leaves the work tree unchanged. This differs from Git’s `clean.requireForce` error on purpose — previewing is the default so you can see the impact before deleting anything. `--dry-run` is accepted explicitly (and conflicts with `-f`).

Pass `-f` to actually delete the listed paths. Pass `--ignored` to also remove ignored files (similar to `git clean -x`). Pass `--only-ignored` to remove only ignored paths (similar to `git clean -X`); it cannot be combined with `--ignored`.

Use `-e` / `--exclude` (repeatable) to add exclude patterns: matching paths are not removed (like `git clean -e`). Use `--files-only` to skip untracked directories (like `git clean` without `-d`).

With path arguments, only untracked paths matching those pathspecs are considered.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Limit cleaning to these paths. Omit to scan the whole tree. |
| `--pathspec-from-file` | Read pathspecs from a file, or `-` for stdin (same rules as Git). |
| `--pathspec-file-nul` | With `--pathspec-from-file`, entries are NUL-separated instead of line-based. |
| `-f`, `--force` | Delete untracked paths instead of only listing them. Conflicts with `--dry-run`. |
| `--dry-run` | Explicit preview (default when `-f` is not passed). Conflicts with `-f`. |
| `--ignored` | Remove ignored files and directories as well (`git clean -x`). Conflicts with `--only-ignored`. |
| `--only-ignored` | Remove only ignored paths (`git clean -X`). Conflicts with `--ignored`. |
| `-e`, `--exclude` | Additional exclude pattern (repeatable). Matching paths are kept. |
| `--files-only` | Remove untracked files but not directories. |

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

Remove only ignored files:

```console
$ grit clean --only-ignored
Would remove build/output.tmp
```

## JSON output

Schema: [v1](/schemas/clean/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `dry_run` | boolean | `true` when `-f` was not passed (preview only); `false` when paths were actually deleted. |
| `removed` | array of strings | Repository-relative paths removed, or that would be removed when `-f` was not passed. |
| `scope` | string | `untracked`, `untracked_and_ignored`, or `only_ignored`. |

Example (dry run):

```json
{
  "dry_run": true,
  "scope": "untracked",
  "removed": [
    "untracked.txt",
    "nested/"
  ]
}
```

Example (with `-f`):

```json
{
  "dry_run": false,
  "scope": "untracked",
  "removed": [
    "untracked.txt",
    "nested/"
  ]
}
```

## Markdown output

Schema: [v1](/schemas/clean/v1.json)
Pass [`--markdown`](../global-options/) for a bullet list of paths under **Would remove** or **Removed**.

## See also

[grit status](../status/), [grit add](../add/)
