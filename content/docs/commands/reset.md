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
grit reset --keep [<rev>]
grit reset --dry-run [<rev>]
```

## Description

Moves the current branch (or detached `HEAD`) to `<rev>`, which defaults to `HEAD`. Use this to uncommit while keeping changes (`--soft`), drop commits from the tip but keep your files in the worktree (`--mixed`, the default), discard local changes entirely (`--hard`), or move the tip while merging index and worktree updates toward the target but keeping unrelated local edits (`--keep`).

`--soft` updates the branch ref only: staged and unstaged files stay as they are. That is the usual way to “uncommit” the last commit while keeping everything staged.

`--mixed` moves the tip and resets the index to match `<rev>`; the worktree is not touched.

`--hard` moves the tip, resets the index, and updates tracked files in the worktree to match `<rev>`. Untracked files are left alone. Paths that were unmerged when the reset started and are absent from `<rev>` are removed from the worktree (including files another side introduced during a conflict). Intent-to-add paths stay on disk as untracked files, as in Git.

In a blobless clone, `--hard` batches missing blobs needed to materialize or repair tracked paths into one promisor fetch. This includes `reset --hard HEAD` after `clone --blobs none --no-checkout`, and unchanged paths missing from the worktree when resetting to another commit. Repeated blob ids are requested once; sparse-excluded paths and submodule gitlinks do not download blob content. The usual partial-clone lazy-fetch opt-outs still apply.

In a sparse checkout (`git sparse-checkout`), `--hard` and `--mixed` keep paths outside the sparse patterns out of the worktree: their index entries keep the skip-worktree bit, the same as `git reset`. Hard reset handles tracked directories replaced by files or symlinks, including directories with materialized files outside the sparse patterns. Files reached through a symlinked parent are left untouched.

`--keep` moves the tip and updates paths that differ between the current commit and `<rev>`, but refuses when a path that would change also has local staged or unstaged edits. Unrelated local changes (files that do not differ between the commits) are preserved.

`--dry-run` reports what would happen — including paths that `--hard` would discard or `--keep` would refuse on — without moving refs, the index, or the worktree.

A successful reset (any mode except `--dry-run`) ends an in-progress merge, cherry-pick, or revert the same way Git's `reset` does: operation sentinel files such as `MERGE_HEAD` and `CHERRY_PICK_HEAD` are removed.

A reset that moves the tip checks that the branch (or detached `HEAD`) still names the commit the reset started from, so a concurrent update is reported instead of overwritten. The move is appended to the branch's and `HEAD`'s reflogs where `core.logAllRefUpdates` asks for one or the reflog already exists, as Git does.

Every successful reset, in any mode and also to the commit `HEAD` is already on, records the commit `HEAD` was on before it in `ORIG_HEAD` (as `git reset` does, with no reflog), replacing any earlier value. `grit reset ORIG_HEAD` with the same mode undoes the reset, and `grit reset --hard ORIG_HEAD` is the usual way to undo a merge, rebase or pull, which record `ORIG_HEAD` too. `--dry-run` and a refused reset leave `ORIG_HEAD` alone.

When resetting backward, grit lists commits that would leave the branch (not reachable from `<rev>` or any other ref, including branches, tags, remotes, stash, and other worktrees). After a successful reset, human output reports how many commits are no longer on the branch, the previous tip (abbreviated), and how to recover with the same mode you used — for example `grit reset --hard <previous>` after a hard reset, or `grit reset <previous>` after the default mixed reset (also recorded in `ORIG_HEAD`, and in the reflog when reflogs are enabled). With `--dry-run`, the same count uses future tense (“would leave …”) because nothing has moved yet. The check usually stays near the commits being left behind, so undoing the last commit stays fast on deep repositories; annotated tags are peeled from `packed-refs` when it records their targets, and without a commit-graph the tip commits of hundreds of refs are read in parallel, headers only. Refs recorded in `packed-refs` (on a clone, usually every tag) are not read at all when every commit being left behind is dated more than a day after the file was last written: such a ref named a commit that already existed then, so it cannot reach them. Otherwise, and in repositories with grafts, replace refs or a shallow boundary, those refs are read as above. With a commit-graph the count is exact. Without one, the walk ends by commit date like Git's `git rev-list <previous> --not <refs>`, so under heavy clock skew (a ref that reaches a commit through history dated more than a day before it, or a commit dated more than a day ahead of when it was made) the count can be wrong.

`<rev>` is any revision Grit understands: branch name, `HEAD~1`, full or abbreviated commit id, and so on.

## Options

| Option | Description |
| --- | --- |
| `<rev>` | Commit to reset to. Defaults to `HEAD`. |
| `--soft` | Move the branch tip only. |
| `--mixed` | Move the tip and reset the index (default when no mode flag is given). |
| `--hard` | Move the tip, reset the index, and update the worktree. |
| `--keep` | Move the tip and update differing paths; refuse when local edits block a tree change. |
| `--dry-run` | Preview the reset without changing refs, index, or worktree. |
| `-n`, `--limit` | Cap the `orphaned_commits` list in `--json` output (default 20). |

## Examples

Uncommit the last commit but keep changes staged:

```console
$ grit reset --soft HEAD~1
Reset refs/heads/main to a1b2c3d (soft)
Warning: 1 commit is no longer on refs/heads/main (was d4e5f6a). Recover it with `grit reset --soft d4e5f6a` (recorded in ORIG_HEAD and, when reflogs are on, the reflog).
```

Hard reset two commits back, then follow the recovery hint to restore the branch and worktree:

```console
$ grit reset --hard HEAD~2
Reset refs/heads/main to a1b2c3d (hard)
Warning: 2 commits are no longer on refs/heads/main (was f6a7b8c). Recover them with `grit reset --hard f6a7b8c` (recorded in ORIG_HEAD and, when reflogs are on, the reflog).
$ grit reset --hard f6a7b8c
Reset refs/heads/main to f6a7b8c (hard)
```

Move the tip back one commit and unstage, keeping file contents:

```console
$ grit reset HEAD~1
Reset main to a1b2c3d (mixed)
```

Preview a hard reset without discarding work:

```console
$ grit reset --dry-run --hard HEAD~1
Would reset main to a1b2c3d (hard)
Would discard local changes in:
  tracked.txt
```

## JSON output

Schema: [v1](/schemas/reset/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `mode` | string | `soft`, `mixed`, `hard`, or `keep`. |
| `target` | string | Full hex oid of the commit `HEAD` will point at (or would, when `dry_run` is true). |
| `previous` | string | Full hex oid before the reset. |
| `ref` | string | Branch ref updated, or `HEAD` when detached. |
| `dry_run` | boolean | True when nothing was written. |
| `would_overwrite` | string[] | With `--dry-run --hard`: tracked paths whose local edits would be discarded. |
| `would_block` | string[] | With `--dry-run --keep`: paths that would make the reset refuse. |
| `orphaned_commits` | object[] | Sample of commits that would leave the branch (`oid`, `change_id`, `subject`). |
| `orphaned_total` | number | Advisory unreachable count (may exceed `orphaned_commits.length`). Without a commit-graph, severe clock skew can make this inaccurate; it never controls the reset. |
| `orphaned_truncated` | boolean | True when `-n`/`--limit` capped the orphan list. |

Example:

```json
{
  "mode": "soft",
  "target": "062ca6e74f5603f4d09063c0f46cd07d8c96e019",
  "previous": "83352a61f6715ec3f0a297c6d6150725154fb833",
  "ref": "refs/heads/main",
  "dry_run": false,
  "would_overwrite": [],
  "would_block": [],
  "orphaned_commits": [
    {
      "oid": "83352a61f6715ec3f0a297c6d6150725154fb833",
      "change_id": "xzyqzyyzswyupvtlsovyzultryuqzlvk",
      "subject": "second"
    }
  ],
  "orphaned_total": 1,
  "orphaned_truncated": false
}
```

## Markdown output

Schema: [v1](/schemas/reset/v1.json)
Pass `--markdown` for agent-friendly output: a short heading plus bullet list of ref, mode, target, previous, dry-run, and orphan summary.



```text
## Reset

- **ref:** `refs/heads/main`
- **mode:** `soft`
- **target:** `062ca6e74f5603f4d09063c0f46cd07d8c96e019`
- **previous:** `83352a61f6715ec3f0a297c6d6150725154fb833`
- **dry_run:** `false`
- **orphaned_total:** `1` (truncated: `false`)
- **recovery:** `grit reset --soft 83352a61f6715ec3f0a297c6d6150725154fb833` restores it (also recorded in ORIG_HEAD; reflog when enabled)
```

Resetting to default `HEAD` on an unborn branch reports `branch_no_commits` (same message as [`grit log`](log.md)) instead of an opaque revision error.

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

When another process holds `.git/index.lock`, `--hard`, `--keep` and the default mixed reset fail with exit `1` (`kind` `lock_held` under `--json`) before touching anything: `HEAD`, the branch, the index and the worktree are unchanged. Retry once the other process finishes.


## See also

[grit switch](../switch/), [grit restore](../restore/), [grit commit](../commit/)
Sparse indexes keep unchanged excluded directory trees compact during checkout.
Only affected directories or directories present outside the sparse cone need
individual index rows. Performance and scaling methodology are documented in
[Benchmarks](../benchmarks.md).

Hard reset reapplies the sparse definition: tracked files that appeared outside
the cone are removed again. `sparse.expectFilesOutsideOfPatterns=true` preserves
outside files whose index entries still have skip-worktree set. Keep-reset uses
the full worktree guard checks for unmerged indexes.
