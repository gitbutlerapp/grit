---
title: grit restore
summary: Restore working tree and/or index paths from HEAD, the index, or another revision.
group: Making changes
order: 3
---

## Synopsis

```text
grit restore [--staged] [--worktree] [--source=<rev>] -- <path>…
```

## Description

Discards local changes for tracked paths you name. By default, `grit restore` resets the **working tree** to match the **index** (the same idea as unstaging worktree edits while keeping the index as-is).

- `--staged` resets the **index** from `HEAD` (or from `--source` when you pass it). On an unborn branch, staged paths are removed from the index.
- `--worktree` resets the **working tree**. When you omit both flags, only the worktree is restored.
- `--source=<rev>` reads content from a commit or tree instead of the usual default (`HEAD` for `--staged`, the index for the worktree).
- Paths that are missing in the source are removed from the target. Untracked files are never touched.

## Options

| Option | Description |
| --- | --- |
| `<path>…` | One or more pathspecs (required). |
| `-S`, `--staged` | Restore the index. |
| `-W`, `--worktree` | Restore the working tree. |
| `--source` | Commit or tree to restore from. |

## Examples

Discard unstaged edits to a file (worktree ← index):

```console
$ grit restore README.md
Restored README.md
```

Unstage a file (index ← HEAD):

```console
$ grit restore --staged README.md
Restored README.md
```

Restore the working tree from the previous commit:

```console
$ grit restore --source=HEAD~1 -- README.md
Restored README.md
```

## JSON output

Pass `--json` for scripting:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `restored` | array of strings | Paths created or updated. |
| `removed` | array of strings | Paths removed because the source had no entry. |

Example:

```json
{
  "restored": ["README.md"],
  "removed": []
}
```

## Markdown output

Pass `--markdown` for agent-friendly bullet lists of restored and removed paths.

## See also

[grit add](../add/), [grit status](../status/)
