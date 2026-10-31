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

`grit co` does the same thing, and so does `grit checkout <branch>`; `grit checkout <url>` makes a [snapshot checkout](../checkout/) from a server instead.

## Description

Switches to a local branch, creates a branch from a remote-tracking ref or start point, or checks out a commit in detached `HEAD` mode. Updates tracked files in the working tree to match the destination.

Local edits are allowed when they only touch paths that are the same on both sides of the switch, or when a changed path already matches the destination tree in the index. Grit still refuses when your changes would conflict with files that differ between branches, or when an untracked file would be overwritten.

After a [`clone --no-checkout`](../clone/), the first switch creates the index and materializes the entire destination tree. This works with clones created by grit or system Git, including switching to the current branch, detaching at `HEAD`, or creating a branch at a commit with the same tree. The previous `HEAD` is still used for reflogs and hooks. Untracked files at destination paths block the switch. An existing empty index continues to represent staged deletions and receives the normal local-change protection.

Use `--dry-run` to run the same overwrite checks and report how many paths would change without updating `HEAD`, the index, the worktree, or reflogs. Agents can preflight a switch with `--json --dry-run` before mutating the tree.

On the first switch after `clone --no-checkout`, the dry-run count includes every path to materialize, including files unchanged between the old and new commits.

A switch either completes or changes nothing. Pressing Ctrl+C, or sending `SIGTERM` or `SIGHUP` on Unix, while files are being checked out stops the switch with `error: operation cancelled`; `HEAD`, the index and the work tree are left exactly as they were, so `grit status` stays clean, no `.git/index.lock` is left behind and the same switch can simply be run again. This works the same in a Windows console, where Ctrl+Break counts as Ctrl+C. A second interrupt terminates grit at once, without that cleanup; only `SIGKILL` or closing the console window skips rollback. In the rare case that a path cannot be put back (for example a directory that became read-only), the error has kind `checkout_incomplete` and lists those paths in `paths`; `HEAD` and the index still did not move.

Switching is refused while a merge, rebase, pick, revert or `am` is paused, whether grit or `git` started it: those operations finish by committing onto the current `HEAD`, so moving it would land their result on the wrong branch. When grit knows how the operation finishes or is cancelled (merge, rebase, pick, revert), the error names those commands. `--dry-run` is refused too. A bisect does not block a switch.

A local branch can only be checked out in one worktree at a time (including linked worktrees created with `git worktree add`). Switching to a branch that is already checked out elsewhere fails.

With `-c`, creates the branch at the current commit (or at `<start-point>` when you pass one), then switches to it.

When `<branch>` names a remote-tracking branch (for example `origin/feature`), Grit creates `feature` at the same commit and switches to it if no local branch exists yet. It also records upstream tracking in the repository config (`branch.feature.remote` and `branch.feature.merge`), the same keys `git switch` writes, so `grit status`, `grit pull`, and `grit push` use the matching remote branch.

`--detach` checks out `<rev>` without moving a branch tip. Use another branch or `grit switch <branch>` to reattach.

In a sparse checkout (`git sparse-checkout`), only paths inside the sparse patterns are written to the worktree. Paths outside them are recorded in the index with the skip-worktree bit, the same as `git switch`. The same applies to [`grit merge`](../merge/), [`grit pull`](../pull/) and [`grit reset`](../reset/).

After a real switch (not `--dry-run`), grit runs the `post-checkout` hook with the previous `HEAD` commit, the new one, and `1`, as Git does. The old id is all zeros when switching away from a branch with no commits. The hook also runs when you switch to the branch or commit you are already on. Its exit status never changes the result; a hook that cannot be started prints a `hook_not_run` warning. See [`grit hooks`](../hooks/).

Leaving a detached `HEAD` that carries commits of its own (made with `grit commit` after `--detach`) would leave those commits on no branch, recoverable only through `grit reflog` until garbage collection removes them. Grit still switches, but warns on stderr with the commits it leaves behind (newest first, up to 20) and a `grit branch <name> <oid>` command that keeps them. A commit counts as left behind when no ref, the stash, another worktree's `HEAD`, or the switch destination reaches it, so `grit switch -c <new>` from the detached commit leaves nothing behind. Without a commit-graph the check ends by commit date like Git's `rev-list`, so heavy clock skew can make the count wrong (see [grit reset](../reset/)). `--dry-run` reports the same list without switching. Leaving a branch never warns: the branch still holds its commits.

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
hooks: post-checkout

$ grit switch -
Switched to branch feature

$ grit switch -c br2 origin/br2
Created and switched to branch br2

$ grit switch --detach HEAD~1
HEAD detached at a1b2c3d

$ grit switch other --dry-run
Would switch to other (3 path(s) would change)
```

When you leave a detached `HEAD` with commits no branch holds:

```console
$ grit switch main
Switched to branch main
Warning: 1 commit left behind, not on any branch:
  85ffcb0 work done on detached HEAD
Keep it on a branch with: grit branch <name> 85ffcb090615556eb238400c577de383ca034213
```

When local changes would conflict with the destination branch:

```console
$ grit switch other
error: you have uncommitted changes — commit them before switching
  notes.txt
hint: commit them with `grit commit`, or set them aside with `grit stash`, then try again
```

When `grit switch -` has no previous branch to return to (for example right after `grit init`):

```console
$ grit switch -
error: there is no previous branch to go back to
```

When no branch has that name:

```console
$ grit switch nonexistent
error: no branch named 'nonexistent'
hint: `grit branch` lists your branches; create this one with `grit switch -c nonexistent`
```

While a rebase that `git` started is stopped on a conflict:

```console
$ grit switch main
error: cannot switch branches while an interactive rebase is in progress; finish it with `git rebase --continue` or cancel it with `grit rebase --abort`
```

When an untracked file would be overwritten:

```console
$ grit switch other
error: untracked file 'new.txt' would be overwritten — move or remove it first
  new.txt
hint: move or remove those files, then try again
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
| `hooks_run` | array of strings | Hooks that ran after the switch (`post-checkout` when installed); empty on `--dry-run`. |
| `stranded_commits` | array | Commits only the previous detached `HEAD` reached, newest first, at most 20. Each has `oid`, `change_id` and `subject`. Omitted when nothing is left behind. |
| `stranded_total` | integer | Advisory count of commits left behind (may exceed the length of `stranded_commits`). Without a commit-graph, severe clock skew can make this inaccurate; it never controls the switch. Omitted when zero. |
| `stranded_truncated` | boolean | `true` when `stranded_commits` was capped (omitted when false). |
| `path_collisions` | array of strings | Index paths that collided during checkout on a case-insensitive filesystem. Omitted when empty. |

On a case-insensitive filesystem (`core.ignorecase`), when the destination tree contains paths that differ only by case (or other spellings that map to one on-disk file), grit prints a **warning** on **stderr** in human mode listing every colliding index path, the same message as [`grit clone`](../clone/). Only one path from each group exists on disk; status and diff treat index rows whose blob does not match the shared dentry as modified.

Example (branch switch):

```json
{
  "branch": "feature",
  "created": true,
  "detached": false,
  "hooks_run": ["post-checkout"]
}
```

Example (`--dry-run`):

```json
{
  "branch": "other",
  "created": false,
  "detached": false,
  "dry_run": true,
  "changed_paths": 3,
  "hooks_run": []
}
```

Example (detached):

```json
{
  "branch": null,
  "created": false,
  "detached": true,
  "commit": "a1b2c3d4e5f6789012345678901234567890abcd",
  "hooks_run": []
}
```

Example (leaving a detached `HEAD` with one commit of its own):

```json
{
  "branch": "main",
  "created": false,
  "detached": false,
  "hooks_run": [],
  "stranded_commits": [
    {
      "oid": "85ffcb090615556eb238400c577de383ca034213",
      "change_id": "zrmsknstkznuqxynsolvwoovmolrxwum",
      "subject": "work done on detached HEAD"
    }
  ],
  "stranded_total": 1
}
```

On overwrite refusal with `--json`, stdout is a single object with `kind` `would_overwrite` and `would_overwrite.reason` (`local_changes` or `untracked`) plus a sorted `paths` array, and a `hint`, for example `{"error":"you have uncommitted changes — commit them before switching","hint":"…","kind":"would_overwrite","would_overwrite":{"paths":["notes.txt"],"reason":"local_changes"}}`. See the [error schema](/schemas/error/v1.json).

A branch that does not exist is `kind` `branch_not_found` with a `hint`.

With `--create`, an existing name is `kind` `branch_already_exists`, an invalid name is `branch_invalid_name`, and an empty repository with no start point is `branch_no_commits`. A start point that does not resolve is `revision_not_found`; one that resolves to a non-commit object is a plain error string with no dedicated `kind`.

While a merge, rebase, pick or revert is paused, the refusal is `{"error": "interactive rebase in progress", "kind": "operation_in_progress"}` (the `error` names the paused operation).

## Markdown output

Schema: [v2](/schemas/switch/v2.json) · [v1](/schemas/switch/v1.json)
Pass [`--markdown`](../global-options/) for a `Switch` heading and one summary line, plus a `- Hooks:` line when a hook ran:

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

Leaving commits behind adds a warning block after the summary line:

```text
- Switched to **`main`**

> **Warning:** 1 commit left behind, not on any branch:

- `85ffcb0` work done on detached HEAD

Keep it with `grit branch <name> 85ffcb090615556eb238400c577de383ca034213`.
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

When another process holds `.git/index.lock`, a switch that would update the worktree fails with exit `1` (`kind` `lock_held` under `--json`) before touching anything: the worktree, index, `HEAD` and branches are unchanged. `--dry-run` never takes the lock.


## See also

[grit branch](../branch/), [grit reset](../reset/), [grit merge](../merge/), [grit status](../status/)
Sparse indexes keep unchanged excluded directory trees compact during checkout.
Only affected directories or directories present outside the sparse cone need
individual index rows. Performance and scaling methodology are documented in
[Benchmarks](../benchmarks.md).

Unmerged indexes use the full worktree guard checks.
