---
title: grit restore
summary: Restore working tree and/or index paths from HEAD, the index, or another revision.
group: Making changes
order: 3
---

## Synopsis

```text
grit restore [--staged] [--worktree] [--source=<rev>] [--ours|--theirs|--merge] [--conflict=merge|diff3|zdiff3] [--pathspec-from-file <file|->] [--pathspec-file-nul] -- <path>…
```

## Description

Discards local changes for tracked paths you name. By default, `grit restore` resets the **working tree** to match the **index** (the same idea as unstaging worktree edits while keeping the index as-is).

- `--staged` resets the **index** from `HEAD` (or from `--source` when you pass it). On an unborn branch, staged paths are removed from the index.
- `--worktree` resets the **working tree**. When you omit both flags, only the worktree is restored.
- `--source=<rev>` reads content from a commit or tree instead of the usual default (`HEAD` for `--staged`, the index for the worktree).
- Paths that are missing in the source are removed from the target. Untracked files are never touched.
- During a merge conflict, `--ours` and `--theirs` write index stage 2 or 3 into the worktree (filters and line endings apply) and leave the index unmerged. `--merge` rebuilds conflict markers from stages 1–3; `--conflict` picks the marker style and implies `--merge`. These options cannot be combined with `--staged` or `--source`.

## Options

| Option | Description |
| --- | --- |
| `<path>…` | One or more pathspecs (required unless `--pathspec-from-file` is set). |
| `--pathspec-from-file` | Read pathspecs from a file, or `-` for stdin (same rules as Git). |
| `--pathspec-file-nul` | With `--pathspec-from-file`, entries are NUL-separated instead of line-based. |
| `-S`, `--staged` | Restore the index. |
| `-W`, `--worktree` | Restore the working tree. |
| `--source` | Commit or tree to restore from. |
| `--ours` | Unmerged paths only: check out our side (stage 2) in the worktree. |
| `--theirs` | Unmerged paths only: check out their side (stage 3) in the worktree. |
| `--merge` | Unmerged paths only: recreate conflict markers in the worktree (default style from `merge.conflictStyle`). |
| `--conflict` | With `--merge`: `merge`, `diff3`, or `zdiff3` marker style. |

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

During a merge, inspect one side or bring conflict markers back:

```console
$ grit restore --ours -- README.md
Restored README.md

$ grit restore --merge --conflict=diff3 -- README.md
Restored README.md
```

## JSON output

Schema: [v1](/schemas/restore/v1.json)

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

Schema: [v1](/schemas/restore/v1.json)
Pass `--markdown` for agent-friendly bullet lists of restored and removed paths.

## See also

[grit add](../add/), [grit status](../status/)
