---
title: grit revert
summary: Undo commits with new inverse commits.
group: Branches and tags
order: 5
---

## Synopsis

```text
grit revert [<commit>...]
grit revert --check [<commit>...]
grit revert --continue | --abort | --skip
```

## Description

Creates new commits on the current branch that undo the changes introduced by each listed commit. Reverts run **newest first** within a revision range, matching `git revert`. Each revert commit uses the standard `Revert "…"` subject and a `This reverts commit <oid>.` body (unless you pass `-m` / `--message-file` for a single commit), and gets a new `change-id`.

`<commit>` can be a full or short commit id, a branch name, a range such as `v1.0..main`, or several commits in one invocation.

When a step would conflict, grit stops with conflict markers in the worktree and unmerged index entries — the same model as [`grit pick`](../pick/) and [`grit merge`](../merge/). Resolve conflicts, stage the result with [`grit add`](../add/), then run `grit revert --continue`. Use `--abort` to restore the branch to its pre-revert state, or `--skip` to drop the current step and continue with any remaining commits.

Merge commits require `--mainline <n>` (1-based parent number, long option only) so grit knows which parent line to replay.

## Options

| Option | Description |
| --- | --- |
| `<commit>…` | Commits or revision ranges to revert (required unless continuing). |
| `--mainline <n>` | Parent number when reverting a merge commit. |
| `--continue` | Finish a revert stopped on conflicts or an empty commit. |
| `--abort` | Cancel the in-progress revert and restore the saved branch tip. |
| `--skip` | Skip the current commit and continue with the rest of the sequence. |
| `--no-commit` | Apply the revert to the index only; do not create a commit yet. |
| `--empty stop\|keep\|drop` | When a step would not change the tree: stop (default), record an empty commit, or skip the step. |
| `-m`, `--message` | Replace the revert message (**single commit only**). |
| `--message-file` | Read the revert message from a file (**single commit only**). |
| `--check` | Predict whether the revert would succeed without changing refs, the index, or the worktree. Exit **0** when every step would apply cleanly; exit **1** when a step would conflict or stop as empty (with `--empty stop`). Conflicts with `--continue`, `--abort`, `--skip`, and `--dry-run`. |
| `--dry-run` | List commits that would be reverted without changing refs or the worktree. Conflicts with `--check`. |
| `-X ours\|theirs` | Auto-resolve replay conflicts: keep the current branch side (`ours`) or the reverted change (`theirs`). |
| `--conflict merge\|diff3\|zdiff3` | Conflict marker style when a step stops with conflicts (default from `merge.conflictStyle`). |

## Examples

Revert the previous commit:

```console
$ grit revert HEAD~1
Reverted a1b2c3d → e4f5g6h Revert "Add feature"
```

Revert a merge (first parent as mainline):

```console
$ grit merge feature
$ grit revert --mainline 1 HEAD
Reverted 9a1c2e0 → b2c3d4e Revert "Merge branch 'feature'"
```

Revert a range (newest commit in the range first):

```console
$ grit revert release~5..release
Reverted f1e2d3c → … Revert "Prepare release"
Reverted a1b2c3d → … Revert "Bump version"
```

When the revert conflicts:

```console
$ grit revert feature
CONFLICT (content): f.txt

Revert stopped on conflicts; fix them and run `grit revert --continue`, or `grit revert --abort` / `grit revert --skip`.
```

## JSON output

Schema: [v1](/schemas/revert/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `completed`, `clean`, `conflicts`, `aborted`, or `empty`. |
| `source` | string or null | Source commit for the last revert created when `result` is `completed`. |
| `oid` | string or null | New revert commit when `result` is `completed`, or the predicted final tree oid when `result` is `clean`. |
| `subject` | string or null | Subject of the last revert commit when `result` is `completed`. |
| `change_id` | string or null | Stored `change-id` on the last revert commit, or `null`. |
| `commits` | array | Each revert commit created in this invocation (newest first). |
| `dropped` | array | Source commits skipped as empty per `--empty drop`. |
| `conflicts` | array or null | Conflicted paths when `result` is `conflicts`. |
| `next_steps` | array or null | Suggested follow-up commands when paused. |
| `alternatives` | array or null | `--abort` / `--skip` when paused. |
| `dry_run` | boolean or null | True when `--dry-run` was used. |
| `check` | boolean | True when `--check` was used (nothing was reverted). |

Example:

```json
{
  "result": "completed",
  "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "oid": "a8f2c1d0e9b876543210abcdef0123456789abcd",
  "subject": "Revert \"Add feature\"",
  "commits": [
    {
      "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "oid": "a8f2c1d0e9b876543210abcdef0123456789abcd",
      "subject": "Revert \"Add feature\"",
      "change_id": "zzzzzzzz"
    }
  ],
  "dropped": []
}
```

When `result` is `conflicts`, exit code is **1** and the object includes `conflicts`, `next_steps`, and `alternatives` (same shape as [`grit merge`](../merge/) conflicts).

## Markdown output

Schema: [v1](/schemas/revert/v1.json)

Pass [`--markdown`](../global-options/) for a `Revert` heading and one bullet per reverted commit:

```text
- `9244cbd` → `bbbca73` — Revert "third"
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Revert completed or `--check` predicts success. |
| `1` | Conflicts, empty revert, or other failure. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit pick](../pick/), [grit merge](../merge/)