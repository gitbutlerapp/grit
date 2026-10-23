---
title: grit restore
summary: Restore working tree and/or index paths from HEAD, the index, or another revision.
group: Making changes
order: 3
---

## Synopsis

```text
grit restore [--staged] [--worktree] [--hunks <id,...>] [--lines <path:start-end>] [--dry-run] [--source=<rev>] [--ours|--theirs|--merge] [--conflict=merge|diff3|zdiff3] [--pathspec-from-file <file|->] [--pathspec-file-nul] -- <path>…
```

## Description

Discards local changes for tracked paths you name. By default, `grit restore` resets the **working tree** to match the **index** (the same idea as unstaging worktree edits while keeping the index as-is).

- `--staged` resets the **index** from `HEAD` (or from `--source` when you pass it). On an unborn branch, staged paths are removed from the index.
- `--worktree` resets the **working tree**. When you omit both flags, only the worktree is restored.
- `--hunks` and `--lines` reverse-apply only the selected diff hunks instead of whole files. Unstaged ids come from `grit diff --unstaged --json`; with `--staged`, ids come from `grit diff --staged --json`. Line ranges use new-side line numbers from `grit diff --json` (`lines[].new`), same as `grit add --lines`. Cannot be combined with `--source`, `--ours`, `--theirs`, or `--merge`.
- `--dry-run` with `--hunks` or `--lines` validates selection and reports paths without writing the worktree or index.
- `--source=<rev>` reads content from a commit or tree instead of the usual default (`HEAD` for `--staged`, the index for the worktree).
- Paths that are missing in the source are removed from the target. Untracked files are never touched.
- During a merge conflict, `--ours` and `--theirs` write index stage 2 or 3 into the worktree (filters and line endings apply) and leave the index unmerged. `--merge` rebuilds conflict markers from stages 1–3; `--conflict` picks the marker style and implies `--merge`. These options cannot be combined with `--staged` or `--source`.

## Options

| Option | Description |
| --- | --- |
| `<path>…` | One or more pathspecs (required for full-file restore unless `--pathspec-from-file` is set). Optional with `--hunks` / `--lines` to narrow the diff scan. |
| `--pathspec-from-file` | Read pathspecs from a file, or `-` for stdin (same rules as Git). |
| `--pathspec-file-nul` | With `--pathspec-from-file`, entries are NUL-separated instead of line-based. |
| `-S`, `--staged` | Restore the index (or unstage selected hunks with `--hunks` / `--lines`). |
| `-W`, `--worktree` | Restore the working tree (default when `--staged` is absent). |
| `--hunks` | Discard or unstage only the listed hunk ids (comma-separated, repeatable). |
| `--lines` | Discard or unstage only new-side line ranges (`path:start-end`, repeatable). Mix with `--hunks`. |
| `--dry-run` | With `--hunks` or `--lines`, report paths that would change without writing. |
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

Discard one unstaged hunk by id:

```console
$ grit diff --unstaged --json -- f.txt | jq -r '.files[0].hunks[0].id'
a1b2c3d4e5f6
$ grit restore --hunks a1b2c3d4e5f6 -- f.txt
Restored f.txt
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
| `restored_hunks` | object | When `--hunks` was used: map of path → hunk id strings. |
| `restored_lines` | object | When `--lines` was used: map of path → range strings (`start` or `start-end`). |
| `dry_run` | boolean | True when `--dry-run` was passed with hunk/line selection. |

Example:

```json
{
  "restored": ["README.md"],
  "removed": [],
  "restored_hunks": {},
  "restored_lines": {},
  "dry_run": false
}
```

## Markdown output

Schema: [v1](/schemas/restore/v1.json)
Pass `--markdown` for agent-friendly bullet lists of restored and removed paths.

## See also

[grit add](../add/), [grit status](../status/)
