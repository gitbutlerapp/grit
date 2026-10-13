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
```

`grit checkout` and `grit co` do the same thing.

## Description

Switches to a local branch, creates a branch from a remote-tracking ref or start point, or checks out a commit in detached `HEAD` mode. Updates tracked files in the working tree to match the destination.

Local edits are allowed when they only touch paths that are the same on both sides of the switch, or when a changed path already matches the destination tree in the index. Grit still refuses when your changes would conflict with files that differ between branches, or when an untracked file would be overwritten.

A local branch can only be checked out in one worktree at a time (including linked worktrees created with `git worktree add`). Switching to a branch that is already checked out elsewhere fails.

With `-c`, creates the branch at the current commit (or at `<start-point>` when you pass one), then switches to it.

When `<branch>` names a remote-tracking branch (for example `origin/feature`), Grit creates `feature` at the same commit and switches to it if no local branch exists yet.

`--detach` checks out `<rev>` without moving a branch tip. Use another branch or `grit switch <branch>` to reattach.

Updating the working tree uses parallel checkout when the change set is large enough; tune [`checkout.workers`](../../library/parallelism.md) and `checkout.thresholdForParallelism`, or pass global [`--threads`](../global-options.md) to cap or force worker count for this run.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | Local branch to switch to, remote-tracking name to DWIM, or new branch name with `-c`. |
| `-`, `@{-1}` | Switch to the previous branch (from the `HEAD` reflog); fails when there is no prior checkout entry. |
| `<start-point>` | With `-c`, commit to create the branch from (defaults to `HEAD`). |
| `-c`, `--create` | Create the branch first, then switch to it. Fails if the branch already exists. |
| `--detach` | Check out a commit in detached `HEAD` mode instead of switching a branch. |

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
```

When local changes would conflict with the destination branch:

```console
$ grit switch other
error: you have uncommitted changes — commit them before switching
```

## JSON output

Schema: [v1](/schemas/switch/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Short branch name when attached; empty when detached. |
| `created` | boolean | `true` when `-c` or remote DWIM created the branch. |
| `detached` | boolean | `true` when `--detach` was used. |
| `commit` | string | Present when `detached` is true: full hex oid of the checked-out commit. |

Example (branch switch):

```json
{
  "branch": "feature",
  "created": true,
  "detached": false
}
```

Example (detached):

```json
{
  "branch": "",
  "created": false,
  "detached": true,
  "commit": "a1b2c3d4e5f6789012345678901234567890abcd"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for a `Switch` heading and one summary line:

```text
- Switched to **`feature`**
```

Creating a branch:

```text
- Created and switched to **`topic`**
```

Detached HEAD:

```text
- HEAD detached at `9244cbd`
```

## See also

[grit branch](../branch/), [grit reset](../reset/), [grit merge](../merge/), [grit status](../status/)
