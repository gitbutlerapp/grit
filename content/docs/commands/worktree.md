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
grit worktree lock <path> [--reason <text>]
grit worktree unlock <path>
grit worktree prune
```

## Description

Git linked worktrees share one object database but have separate working trees and `HEAD`. `grit worktree list` shows the main worktree first, then linked worktrees (same order as `git worktree list`). Detached checkouts are labeled `(detached HEAD)` in human output (same idea as `git worktree list`). Linked rows whose working tree directory is gone are marked `prunable`, matching `git worktree list`.

Paths in human, `--json`, and `--markdown` output use normal absolute paths (no Windows `\\?\` verbatim prefix) with forward slashes, so scripts can pass them to `cd` or `grit worktree remove` unchanged.

`grit worktree add` creates the admin directory under `.git/worktrees/`, writes the `gitdir` / `commondir` files, checks out the tree, and registers the new worktree. With only a path, grit creates a new branch named after the directory basename at `HEAD`. With a `<rev>` that names an existing branch, that branch is checked out (it must not already be checked out elsewhere). Use `-b` to create a new branch at `<rev>` (default `HEAD`). Use `--detach` for a detached HEAD at the resolved commit.

If `<path>` is already registered but its directory was deleted manually, grit refuses the add (same as Git). Use `-f` / `--force` to replace the stale registration, or `grit worktree prune` / `grit worktree remove` to clear it first.

`grit worktree remove` deletes the linked worktree's admin directory and, with `-f`, the working tree even when it has local changes.

`grit worktree prune` removes admin directories for linked worktrees whose working tree is missing (skips locked worktrees).

`grit worktree lock` writes a `locked` file under the worktree admin directory (same on-disk format as `git worktree lock`). Locked worktrees are skipped by `prune` and refused by `remove` unless `-f`.

`grit worktree unlock` removes that lock file.

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

### `lock`

| Option | Description |
| --- | --- |
| `<path>` | Path to the linked worktree. |
| `--reason` | Optional reason stored in the lock file (shown by `git worktree list`). |

### `unlock`

| Option | Description |
| --- | --- |
| `<path>` | Path to the linked worktree. |

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

Schema: [v1](/schemas/worktree.list/v1.json) · [worktree.add v1](/schemas/worktree.add/v1.json) · [worktree.remove v1](/schemas/worktree.remove/v1.json) · [worktree.prune v1](/schemas/worktree.prune/v1.json) · [worktree.lock v1](/schemas/worktree.lock/v1.json) · [worktree.unlock v1](/schemas/worktree.unlock/v1.json)

Pass `--json` on any subcommand. The top-level `action` is `list`, `add`, `remove`, `prune`, `lock`, or `unlock`.

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
| `action` | string | `list`, `add`, `remove`, `prune`, `lock`, or `unlock`. |
| `worktrees` | array | For `list`, one row per worktree (each item includes `path`, `head`, `branch`, and related flags). |
| `path` | string | For `add` / `remove` / `lock` / `unlock`, absolute path to the working tree. |
| `head` | string | For `add`, current commit oid checked out in the new worktree. |
| `branch` | string or null | For `add`, branch checked out when not detached. |
| `reason` | string or null | For `lock`, optional reason stored in the lock file. |
| `pruned` | array | For `prune`, absolute paths whose registrations were removed. |

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

Lock:

```json
{
  "action": "lock",
  "path": "/tmp/hotfix",
  "reason": "maintenance"
}
```

Unlock:

```json
{
  "action": "unlock",
  "path": "/tmp/hotfix"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for structured output.

Schema: [worktree.list v1](/schemas/worktree.list/v1.json) · [worktree.add v1](/schemas/worktree.add/v1.json) · [worktree.remove v1](/schemas/worktree.remove/v1.json) · [worktree.prune v1](/schemas/worktree.prune/v1.json) · [worktree.lock v1](/schemas/worktree.lock/v1.json) · [worktree.unlock v1](/schemas/worktree.unlock/v1.json)

List shows a table of path, branch, and HEAD:

```text
| Path | Branch | HEAD |
| --- | --- | --- |
| /tmp/repo | `main` | `abc1234` |
| /tmp/hotfix | `hotfix` | `abc1234` |
```

Add and remove use short headings with the path and branch (or detached HEAD) and commit oid.


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit switch](../switch/), [grit branch](../branch/)