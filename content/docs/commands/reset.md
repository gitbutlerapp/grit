---
title: grit reset
summary: Move the branch tip and optionally reset the index or worktree.
group: Branches and tags
order: 3
---

## Synopsis

```text
grit reset [<rev>]
grit reset --soft [<rev>]
grit reset --mixed [<rev>]
grit reset --hard [<rev>]
```

## Description

Moves the current branch (or detached `HEAD`) to `<rev>`, which defaults to `HEAD`. Use this to uncommit while keeping changes (`--soft`), drop commits from the tip but keep your files in the worktree (`--mixed`, the default), or discard local changes entirely (`--hard`).

`--soft` updates the branch ref only: staged and unstaged files stay as they are. That is the usual way to “uncommit” the last commit while keeping everything staged.

`--mixed` moves the tip and resets the index to match `<rev>`; the worktree is not touched.

`--hard` moves the tip, resets the index, and updates tracked files in the worktree to match `<rev>`. Untracked files are left alone.

`<rev>` is any revision Grit understands: branch name, `HEAD~1`, full or abbreviated commit id, and so on.

## Options

| Option | Description |
| --- | --- |
| `<rev>` | Commit to reset to. Defaults to `HEAD`. |
| `--soft` | Move the branch tip only. |
| `--mixed` | Move the tip and reset the index (default when no mode flag is given). |
| `--hard` | Move the tip, reset the index, and update the worktree. |

## Examples

Uncommit the last commit but keep changes staged:

```console
$ grit reset --soft HEAD~1
Reset main to a1b2c3d (soft)
```

Move the tip back one commit and unstage, keeping file contents:

```console
$ grit reset HEAD~1
Reset main to a1b2c3d (mixed)
```

## JSON output

Schema: [v1](/schemas/reset/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `mode` | string | `soft`, `mixed`, or `hard`. |
| `target` | string | Full hex oid of the commit `HEAD` now points at. |
| `previous` | string | Full hex oid before the reset. |
| `ref` | string | Branch ref updated, or `HEAD` when detached. |

Example:

```json
{
  "mode": "soft",
  "target": "a1b2c3d4e5f6789012345678901234567890abcd",
  "previous": "fedcba0987654321fedcba0987654321fedcba09",
  "ref": "main"
}
```

## Markdown output

Pass `--markdown` for agent-friendly output: a short heading plus bullet list of ref, mode, target, and previous oids.

## See also

[grit switch](../switch/), [grit restore](../restore/), [grit commit](../commit/)
