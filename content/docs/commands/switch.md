---
title: grit switch
summary: Switch to another branch, remote branch, or detached commit.
group: Branches and tags
order: 2
---

## Synopsis

```text
grit switch <branch>
grit switch -
grit switch @{-1}
grit switch -c <branch> [<start-point>]
grit switch --detach <rev>
grit switch [--dry-run] …
```

`grit checkout` and `grit co` do the same thing.

## Description

Switches to a local branch, creates a branch from a remote-tracking ref or start point, or checks out a commit in detached `HEAD` mode. Updates tracked files in the working tree to match the destination.

Local edits are allowed when they only touch paths that are the same on both sides of the switch, or when a changed path already matches the destination tree in the index. Grit still refuses when your changes would conflict with files that differ between branches, or when an untracked file would be overwritten.

Use `--dry-run` to run the same overwrite checks and report how many paths would change without updating `HEAD`, the index, the worktree, or reflogs. Agents can preflight a switch with `--json --dry-run` before mutating the tree.

A local branch can only be checked out in one worktree at a time (including linked worktrees created with `git worktree add`). Switching to a branch that is already checked out elsewhere fails.

With `-c`, creates the branch at the current commit (or at `<start-point>` when you pass one), then switches to it.

When `<branch>` names a remote-tracking branch (for example `origin/feature`), Grit creates `feature` at the same commit and switches to it if no local branch exists yet. It also records upstream tracking in the repository config (`branch.feature.remote` and `branch.feature.merge`), the same keys `git switch` writes, so `grit status`, `grit pull`, and `grit push` use the matching remote branch.

`--detach` checks out `<rev>` without moving a branch tip. Use another branch or `grit switch <branch>` to reattach.

Updating the working tree uses parallel checkout when the change set is large enough; tune [`checkout.workers`](../../library/parallelism.md) and `checkout.thresholdForParallelism`. On non-Windows hosts, global [`--threads`](../global-options.md) can cap checkout workers for this run; on **Windows**, checkout stays serial until `checkout.workers` is set in config (see [parallelism](../../library/parallelism.md)).

## Options

| Option | Description |
| --- | --- |
| `<branch>` | Local branch to switch to, remote-tracking name to DWIM, or new branch name with `-c`. |
| `-`, `@{-1}` | Switch to the previous branch (from the `HEAD` reflog); fails when there is no prior checkout entry. |
| `<start-point>` | With `-c`, commit to create the branch from (defaults to `HEAD`). |
| `-c`, `--create` | Create the branch first, then switch to it. Fails if the branch already exists. |
| `--detach` | Check out a commit in detached `HEAD` mode instead of switching a branch. |
| `--dry-run` | Run overwrite checks and report how many paths would change; do not update refs, the index, or the worktree. |

## Examples

```console
$ grit switch -c feature
Created and switched to branch feature

$ grit switch main
Switched to branch main

$ grit switch -
Switched to branch feature

$ grit switch -c br2 origin/br2
Created and switched to branch br2

$ grit switch --detach HEAD~1
HEAD detached at a1b2c3d

$ grit switch other --dry-run
Would switch to other (3 path(s) would change)
```

When local changes would conflict with the destination branch:

```console
$ grit switch other
error: you have uncommitted changes — commit them before switching
```

When an untracked file would be overwritten:

```console
$ grit switch other
error: untracked file 'new.txt' would be overwritten — move or remove it first
```

## JSON output

Schema: [v2](/schemas/switch/v2.json) · [v1](/schemas/switch/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string or null | Short branch name `HEAD` is on after the switch; `null` when detached. |
| `created` | boolean | `true` when `-c` or remote DWIM created the branch. |
| `detached` | boolean | `true` when `HEAD` is detached after the switch (`branch` is `null`). |
| `commit` | string or null | Full hex oid when `detached` is true; omitted when attached. |
| `dry_run` | boolean | `true` when `--dry-run` was used (omitted when false). |
| `changed_paths` | integer or null | With `--dry-run`, count of paths that would differ after checkout. |

Example (branch switch):

```json
{
  "branch": "feature",
  "created": true,
  "detached": false
}
```

Example (`--dry-run`):

```json
{
  "branch": "other",
  "created": false,
  "detached": false,
  "dry_run": true,
  "changed_paths": 3
}
```

Example (detached):

```json
{
  "branch": null,
  "created": false,
  "detached": true,
  "commit": "a1b2c3d4e5f6789012345678901234567890abcd"
}
```

On overwrite refusal with `--json`, stdout is a single object with `kind` `would_overwrite` and `would_overwrite.reason` (`local_changes` or `untracked`) plus a sorted `paths` array. See the [error schema](/schemas/error/v1.json).

## Markdown output

Schema: [v2](/schemas/switch/v2.json) · [v1](/schemas/switch/v1.json)
Pass [`--markdown`](../global-options/) for a `Switch` heading and one summary line:

```text
- Switched to **`feature`**
```

Creating a branch:

```text
- Created and switched to **`topic`**
```

Dry run:

```text
- Would switch to **`other`** (3 path(s) would change)
```

Detached HEAD:

```text
- HEAD detached at `9244cbd`
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit branch](../branch/), [grit reset](../reset/), [grit merge](../merge/), [grit status](../status/)