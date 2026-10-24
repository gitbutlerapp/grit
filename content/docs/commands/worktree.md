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
grit worktree add -B <branch> <path> [<rev>]
grit worktree add --detach <path> [<rev>]
grit worktree add --lock [--reason <text>] <path> [<rev>]
grit worktree add --no-checkout <path> [<rev>]
grit worktree add --track <path> [<rev>]
grit worktree add -f <path> [<rev>]
grit worktree remove <path>
grit worktree remove -f <path>
grit worktree lock <path> [--reason <text>]
grit worktree unlock <path>
grit worktree move <worktree> <new-path> [-f] [--dry-run]
grit worktree repair [<path>…] [--dry-run]
grit worktree prune
```

## Description

Git linked worktrees share one object database but have separate working trees and `HEAD`. `grit worktree list` shows the main worktree first, then linked worktrees (same order as `git worktree list`). The current worktree is prefixed with `*` in human output. Detached checkouts are labeled `(detached HEAD)` in human output (same idea as `git worktree list`). Linked rows whose working tree directory is gone are marked `prunable`, matching `git worktree list --porcelain` reason text in JSON.

Paths in human, `--json`, and `--markdown` output use normal absolute paths (no Windows `\\?\` verbatim prefix) with forward slashes, so scripts can pass them to `cd` or `grit worktree remove` unchanged.

`grit worktree add` creates the admin directory under `.git/worktrees/`, writes the `gitdir` / `commondir` files, checks out the tree (unless `--no-checkout`), and registers the new worktree. With only a path, grit creates a new branch named after the directory basename at `HEAD`. With a `<rev>` that names an existing branch, that branch is checked out (it must not already be checked out elsewhere). Use `-b` to create a new branch at `<rev>` (default `HEAD`). Use `-B` to create or reset a branch to `<rev>` (writes a reflog entry when resetting). Use `--detach` for a detached HEAD at the resolved commit. Use `--lock` (optional `--reason`) to write the `locked` file before checkout finishes. Use `--no-checkout` to register `HEAD` without populating the working tree. Use `--track` when the start point is a remote-tracking branch to set `branch.*.remote` / `merge` like `git worktree add --track`.

If `<path>` is already registered but its directory was deleted manually, grit refuses the add (same as Git). Use `-f` / `--force` to replace the stale registration, or `grit worktree prune` / `grit worktree remove` to clear it first.

`grit worktree remove` deletes the linked worktree's admin directory and, with `-f`, the working tree even when it has local changes.

`grit worktree prune` removes admin directories for linked worktrees whose working tree is missing (skips locked worktrees). Use `--dry-run` to list what would be pruned without deleting registrations.

`grit worktree lock` writes a `locked` file under the worktree admin directory (same on-disk format as `git worktree lock`). Locked worktrees are skipped by `prune` and refused by `remove` unless `-f`.

`grit worktree unlock` removes that lock file.

`grit worktree move` relocates a linked worktree and rewrites the worktree `.git` gitfile and admin `gitdir` file atomically (same on-disk layout as `git worktree move`). The main worktree cannot be moved. Locked worktrees require `-f` / `--force` (Git requires `-f` twice; grit uses one force flag). Populated submodules block the move, matching Git.

`grit worktree repair` fixes broken `.git` / `gitdir` links after a manual directory move. With no paths, grit repairs every registered linked worktree from the open repository.

## Options

### `list`

No flags.

### `add`

| Option | Description |
| --- | --- |
| `<path>` | Directory for the new working tree (must be missing or empty). |
| `<rev>` | Commit or branch to check out (default `HEAD`). |
| `-b`, `--branch` | Create and check out a new branch at `<rev>`. |
| `-B` | Create or reset a branch to `<rev>` (refuses when that branch is checked out elsewhere). |
| `--detach` | Detached HEAD at the resolved commit. |
| `--lock` | Write the `locked` file before checkout completes. |
| `--reason` | Reason stored in the lock file (requires `--lock`). |
| `--no-checkout` | Register the worktree without checking out files. |
| `--track` | Set upstream when `<rev>` is a remote-tracking branch. |
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

| Option | Description |
| --- | --- |
| `--dry-run` | Report prunable worktrees without removing registrations. |

### `move`

| Option | Description |
| --- | --- |
| `<worktree>` | Path to the linked worktree to move. |
| `<new-path>` | Destination (must not exist; if it is an existing directory, the worktree basename is appended). |
| `-f`, `--force` | Move even when the worktree is locked. |
| `--dry-run` | Preview the move without renaming or rewriting link files. |

### `repair`

| Option | Description |
| --- | --- |
| `[<path>…]` | Linked worktree roots to repair (default: all linked worktrees). |
| `--dry-run` | List repairs without writing files. |

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

Schema: [v1](/schemas/worktree.list/v1.json) · [worktree.add v1](/schemas/worktree.add/v1.json) · [worktree.remove v1](/schemas/worktree.remove/v1.json) · [worktree.prune v2](/schemas/worktree.prune/v2.json) · [worktree.lock v1](/schemas/worktree.lock/v1.json) · [worktree.unlock v1](/schemas/worktree.unlock/v1.json) · [worktree.move v1](/schemas/worktree.move/v1.json) · [worktree.repair v1](/schemas/worktree.repair/v1.json)

Pass `--json` on any subcommand. The top-level `action` is `list`, `add`, `remove`, `prune`, `lock`, `unlock`, `move`, or `repair`.

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
      "lock_reason": null,
      "prunable": false,
      "prunable_reason": null,
      "is_current": true
    }
  ]
}
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `add`, `remove`, `prune`, `lock`, `unlock`, `move`, or `repair`. |
| `worktrees` | array | For `list`, one object per worktree (fields below). |
| `path` | string | For `add` / `remove` / `lock` / `unlock`, absolute path to the working tree. |
| `from` | string | For `move`, previous worktree path. |
| `to` | string | For `move`, new worktree path. |
| `repairs` | array | For `repair`, objects with `path` and `kind` for each link rewritten. |
| `head` | string | For `add`, current commit oid checked out in the new worktree. |
| `branch` | string or null | For `add`, branch checked out when not detached. |
| `locked` | boolean | For `add`, whether the worktree was created locked. |
| `checked_out` | boolean | For `add`, whether files were checked out (`false` with `--no-checkout`). |
| `dry_run` | boolean | For `prune`, whether registrations were left intact. |
| `reason` | string or null | For `lock`, optional reason stored in the lock file. |
| `pruned` | array | For `prune`, objects with `path` and `reason` for each pruned (or would-be pruned) worktree. |

Each object in `worktrees` (list action) includes `path`, `head`, `branch`, `detached`, `bare`, `locked`, `lock_reason` (always present, `null` when absent), `prunable`, optional `prunable_reason`, and `is_current`.

Add:

```json
{
  "action": "add",
  "path": "/tmp/hotfix",
  "branch": "hotfix",
  "head": "abc1234…",
  "locked": false,
  "checked_out": true
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
  "dry_run": false,
  "pruned": [
    {
      "path": "/tmp/old-wt",
      "reason": "gitdir file points to non-existent location"
    }
  ]
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

Move:

```json
{
  "action": "move",
  "from": "/tmp/linked",
  "to": "/tmp/moved",
  "dry_run": false
}
```

Repair:

```json
{
  "action": "repair",
  "repairs": [
    {
      "path": "/tmp/moved",
      "kind": "gitdir_worktree_path"
    }
  ],
  "dry_run": false
}
```

Repair `kind` values: `git_file_admin_dir`, `gitdir_worktree_path`, `git_file_path_style`, `gitdir_path_style`.

## Markdown output

Pass [`--markdown`](../global-options/) for structured output.

Schema: [worktree.list v1](/schemas/worktree.list/v1.json) · [worktree.add v1](/schemas/worktree.add/v1.json) · [worktree.remove v1](/schemas/worktree.remove/v1.json) · [worktree.prune v2](/schemas/worktree.prune/v2.json) · [worktree.lock v1](/schemas/worktree.lock/v1.json) · [worktree.unlock v1](/schemas/worktree.unlock/v1.json) · [worktree.move v1](/schemas/worktree.move/v1.json) · [worktree.repair v1](/schemas/worktree.repair/v1.json)

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