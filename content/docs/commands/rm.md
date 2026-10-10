---
title: grit rm
summary: Remove tracked paths from the working tree and index.
group: Making changes
order: 3
---

## Synopsis

```text
grit rm [--cached] [-f|--force] <path>...
```

## Description

Removes tracked files or directories from the index and, by default, from the working tree. With `--cached`, the file stays on disk as an untracked file but is removed from the index.

Grit refuses to remove paths that have staged or unstaged local modifications unless you pass `--force`, matching Git's safety rule for `git rm`.

Directory pathspecs remove every tracked file under that directory; trailing slashes on a directory path imply recursive removal.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to remove (required). |
| `--cached` | Remove from the index only; keep the working tree copy. |
| `-f`, `--force` | Remove even when paths have local modifications. |

## Examples

Remove a tracked file:

```console
$ grit rm old.txt
Removed old.txt.
```

Keep the file on disk but stop tracking it:

```console
$ grit rm --cached config.local
Removed config.local.
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `removed` | array of strings | Repository-relative paths removed from the index (and work tree unless `--cached`). |

Example:

```json
{
  "removed": ["old.txt"]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output:

```text
- **removed**:
  - `old.txt`
```

## See also

[grit add](../add/), [grit mv](../mv/), [grit status](../status/)
