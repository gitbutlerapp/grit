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

`--hard` moves the tip, resets the index, and updates tracked files in the worktree to match `<rev>`. Untracked files are left alone.

In a blobless clone, `--hard` batches missing blobs needed to materialize or repair tracked paths into one promisor fetch. This includes `reset --hard HEAD` after `clone --blobs none --no-checkout`, and unchanged paths missing from the worktree when resetting to another commit. Repeated blob ids are requested once; sparse-excluded paths and submodule gitlinks do not download blob content. The usual partial-clone lazy-fetch opt-outs still apply.

In a sparse checkout (`git sparse-checkout`), `--hard` and `--mixed` keep paths outside the sparse patterns out of the worktree: their index entries keep the skip-worktree bit, the same as `git reset`.

`--keep` moves the tip and updates paths that differ between the current commit and `<rev>`, but refuses when a path that would change also has local staged or unstaged edits. Unrelated local changes (files that do not differ between the commits) are preserved.

`--dry-run` reports what would happen — including paths that `--hard` would discard or `--keep` would refuse on — without moving refs, the index, or the worktree.

A successful reset (any mode except `--dry-run`) ends an in-progress merge, cherry-pick, or revert the same way Git's `reset` does: operation sentinel files such as `MERGE_HEAD` and `CHERRY_PICK_HEAD` are removed.

When resetting backward, grit lists commits that would become unreachable (not reachable from `<rev>` or any other ref, including branches, tags, remotes, stash, and other worktrees). A warning is printed when any would be orphaned.

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
Reset main to a1b2c3d (soft)
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
| `orphaned_commits` | object[] | Sample of commits that would become unreachable (`oid`, `change_id`, `subject`). |
| `orphaned_total` | number | Total unreachable commits (may exceed `orphaned_commits.length`). |
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
```

Resetting to default `HEAD` on an unborn branch reports `branch_no_commits` (same message as [`grit log`](log.md)) instead of an opaque revision error.

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit switch](../switch/), [grit restore](../restore/), [grit commit](../commit/)