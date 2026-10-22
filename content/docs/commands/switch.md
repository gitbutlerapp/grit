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
grit switch -C <branch> [<start-point>]
grit switch --orphan <branch>
grit switch --detach <rev>
grit switch [--dry-run] …
```

`grit checkout` and `grit co` do the same thing.

## Description

Switches to a local branch, creates a branch from a remote-tracking ref or start point, or checks out a commit in detached `HEAD` mode. Updates tracked files in the working tree to match the destination.

Local edits are allowed when they only touch paths that are the same on both sides of the switch, or when a changed path already matches the destination tree in the index. Grit still refuses when your changes would conflict with files that differ between branches, or when an untracked file would be overwritten. Pass `--discard-changes` to drop local edits on paths that differ between the current tree and the destination (untracked files outside that diff are never removed).

Use `--dry-run` to run the same overwrite checks and report how many paths would change without updating `HEAD`, the index, the worktree, or reflogs. `--dry-run` is not supported with `--orphan`. Agents can preflight a switch with `--json --dry-run` before mutating the tree.

A local branch can only be checked out in one worktree at a time (including linked worktrees created with `git worktree add`). Switching to a branch that is already checked out elsewhere fails. The same rule applies to `-C` when resetting an existing branch.

With `-c`, creates the branch at the current commit (or at `<start-point>` when you pass one), then switches to it. With `-C`, creates the branch or **resets** an existing branch to the start point, then switches to it (refusing when the branch is checked out in another worktree).

When `<branch>` names a remote-tracking branch (for example `origin/feature`), Grit creates `feature` at the same commit and switches to it if no local branch exists yet and remote guessing is enabled (see `--no-guess`). It records upstream tracking in the repository config (`branch.feature.remote` and `branch.feature.merge`) when tracking is enabled, the same keys `git switch` writes.

`--orphan <name>` points `HEAD` at a new **unborn** branch: the index is cleared and tracked files are removed from the work tree (untracked paths are kept). The branch ref is not created until the first commit.

`--detach` checks out `<rev>` without moving a branch tip. Use another branch or `grit switch <branch>` to reattach.

Updating the working tree uses parallel checkout when the change set is large enough; tune [`checkout.workers`](../../library/parallelism.md) and `checkout.thresholdForParallelism`, or pass global [`--threads`](../global-options.md) to cap or force worker count for this run.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | Local branch to switch to, remote-tracking name to DWIM, or new branch name with `-c`/`-C`. |
| `-`, `@{-1}` | Switch to the previous branch (from the `HEAD` reflog); fails when there is no prior checkout entry. |
| `<start-point>` | With `-c`/`-C`, commit to create or reset the branch from (defaults to `HEAD`). |
| `-c`, `--create` | Create the branch first, then switch to it. Fails if the branch already exists. |
| `-C`, `--force-create` | Create or reset the branch at the start point, then switch to it. |
| `--orphan <name>` | New unborn branch: empty index, tracked files removed, `HEAD` at `refs/heads/<name>`. Fails if the branch already exists. |
| `--discard-changes` | Overwrite local changes on paths that differ between trees; never removes untracked files outside the tree diff. |
| `-t`, `--track` | When creating a branch from a remote-tracking start point, set `branch.<name>.remote` and `.merge`. |
| `--no-track` | Do not set upstream config when creating a branch. |
| `--no-guess` | Do not create a local branch from `origin/<name>` when `<name>` is missing locally (honors `checkout.guess=false` when unset). |
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

$ grit switch -C topic HEAD~1
Switched to branch topic

$ grit switch --orphan fresh
Created and switched to branch fresh

$ grit switch --discard-changes other
Switched to branch other

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

## JSON output

Schema: [v1](/schemas/switch/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Short branch name when attached; empty when detached. |
| `created` | boolean | `true` when `-c`, remote DWIM, or `--orphan` created a new branch context. |
| `detached` | boolean | `true` when `--detach` was used. |
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

Example (detached):

```json
{
  "branch": "",
  "created": false,
  "detached": true,
  "commit": "a1b2c3d4e5f6789012345678901234567890abcd"
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

## Markdown output

Schema: [v1](/schemas/switch/v1.json)
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
