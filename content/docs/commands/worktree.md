---
title: grit worktree
summary: List, add, and remove linked worktrees for the same repository.
group: Branches and tags
order: 6
---

## Synopsis

```text
grit worktree list
grit worktree add <path> [<rev>]
grit worktree add -b <branch> <path> [<rev>]
grit worktree add --detach <path> [<rev>]
grit worktree add -f <path> [<rev>]
grit worktree remove <path>
grit worktree remove -f <path>
grit worktree prune
```

## Description

Git linked worktrees share one object database but have separate working trees and `HEAD`. `grit worktree list` shows the main worktree first, then linked worktrees (same order as `git worktree list`). Linked rows whose working tree directory is gone are marked `prunable`, matching `git worktree list`.

`grit worktree add` creates the admin directory under `.git/worktrees/`, writes the `gitdir` / `commondir` files, checks out the tree, and registers the new worktree. With only a path, grit creates a new branch named after the directory basename at `HEAD`. With a `<rev>` that names an existing branch, that branch is checked out (it must not already be checked out elsewhere). Use `-b` to create a new branch at `<rev>` (default `HEAD`). Use `--detach` for a detached HEAD at the resolved commit.

If `<path>` is already registered but its directory was deleted manually, grit refuses the add (same as Git). Use `-f` / `--force` to replace the stale registration, or `grit worktree prune` / `grit worktree remove` to clear it first.

`grit worktree remove` deletes the linked worktree's admin directory and, with `-f`, the working tree even when it has local changes.

`grit worktree prune` removes admin directories for linked worktrees whose working tree is missing (skips locked worktrees).

## Options

### `list`

No flags.

### `add`

| Option | Description |
| --- | --- |
| `<path>` | Directory for the new working tree (must be missing or empty). |
| `<rev>` | Commit or branch to check out (default `HEAD`). |
| `-b`, `--branch` | Create and check out a new branch at `<rev>`. |
| `--detach` | Detached HEAD at the resolved commit. |
| `-f`, `--force` | Replace a stale registration when the directory is missing. |

### `remove`

| Option | Description |
| --- | --- |
| `<path>` | Path to the linked worktree. |
| `-f`, `--force` | Remove even when locked or dirty. |

### `prune`

No flags.

## Examples

```console
$ grit worktree add ../hotfix -b hotfix main
Created worktree at /tmp/hotfix on branch hotfix

$ grit worktree list
  /tmp/repo
  /tmp/hotfix [hotfix]

$ grit worktree remove ../hotfix -f
Removed worktree /tmp/hotfix

$ grit worktree prune
Pruned worktree /tmp/old-wt
```

## JSON output

Pass `--json` on any subcommand. The top-level `action` is `list`, `add`, `remove`, or `prune`.

List:

```json
{
  "action": "list",
  "worktrees": [
    {
      "path": "/tmp/repo",
      "head": "abc1234…",
      "branch": "main",
      "detached": false,
      "bare": false,
      "locked": false,
      "prunable": false
    }
  ]
}
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, `remove`, or `prune`. |
| `pruned` | array | For `prune`, absolute paths whose registrations were removed. |
| `worktrees` | array | For `list`, one row per worktree. |
| `path` | string | Absolute path to the working tree. |
| `head` | string | Current commit oid for that worktree. |
| `branch` | string | Short branch name when not detached. |
| `detached` | boolean | True when `HEAD` is detached. |
| `bare` | boolean | True only for the main entry of a bare repository. |
| `locked` | boolean | True when the worktree admin dir has a `locked` file. |
| `lock_reason` | string | Optional reason from the `locked` file. |
| `prunable` | boolean | True when the working tree directory is missing (list rows only). |

Add:

```json
{
  "action": "add",
  "path": "/tmp/hotfix",
  "branch": "hotfix",
  "head": "abc1234…"
}
```

Remove:

```json
{
  "action": "remove",
  "path": "/tmp/hotfix"
}
```

Prune:

```json
{
  "action": "prune",
  "pruned": ["/tmp/old-wt"]
}
```

## See also

[grit switch](../switch/), [grit branch](../branch/)
