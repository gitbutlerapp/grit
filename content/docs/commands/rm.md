---
title: grit rm
summary: Remove tracked paths from the working tree and index.
group: Making changes
order: 3
---

## Synopsis

```text
grit rm [--cached] [-f|--force] [--dry-run] [--ignore-unmatch] [--pathspec-from-file <file|->] [--pathspec-file-nul] <path>...
```

## Description

Removes tracked files or directories from the index and, by default, from the working tree. With `--cached`, the file stays on disk as an untracked file but is removed from the index.

Grit refuses to remove paths that have staged or unstaged local modifications unless you pass `--force`, matching Git's safety rule for `git rm`.

Directory pathspecs remove every tracked file under that directory; trailing slashes on a directory path (or a path that names an existing directory) imply recursive removal.

`--dry-run` runs the same checks and lists paths that would be removed without writing the index or work tree. `--ignore-unmatch` treats non-matching pathspecs as success with an empty result (like `git rm --ignore-unmatch`).

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to remove (required unless `--pathspec-from-file` is set). |
| `--pathspec-from-file` | Read pathspecs from a file, or `-` for stdin (same rules as Git). |
| `--pathspec-file-nul` | With `--pathspec-from-file`, entries are NUL-separated instead of line-based. |
| `--cached` | Remove from the index only; keep the working tree copy. |
| `-f`, `--force` | Remove even when paths have local modifications. |
| `--dry-run` | Preview removals without changing the index or work tree. |
| `--ignore-unmatch` | Do not error when a pathspec matches nothing. |

## Examples

Remove a tracked file:

```console
$ grit rm old.txt
Removed old.txt.
```

Preview removal:

```console
$ grit rm --dry-run old.txt
Would remove old.txt.
```

Keep the file on disk but stop tracking it:

```console
$ grit rm --cached config.local
Removed config.local.
```

## JSON output

Schema: [v1](/schemas/rm/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `removed` | array of strings | Repository-relative paths removed from the index (and work tree unless `--cached`), or that would be removed when `--dry-run` is set. |
| `dry_run` | boolean | `true` when `--dry-run` was passed. |

Example:

```json
{
  "dry_run": false,
  "removed": ["old.txt"]
}
```

## Markdown output

Schema: [v1](/schemas/rm/v1.json)
Pass [`--markdown`](../global-options/) for agent-friendly output:

```text
- **Removed**:
  - `old.txt`
```

## See also

[grit add](../add/), [grit mv](../mv/), [grit status](../status/)
