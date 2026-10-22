---
title: grit pick
summary: Copy commits onto the current branch.
group: Branches and tags
order: 4
---

## Synopsis

```text
grit pick [<commit>...]
grit pick --continue | --abort | --skip
```

## Description

Takes the change each listed commit made and applies it to the current branch as new commits (cherry-pick). Each new commit keeps the original author and message (unless you pass `-m` / `--message-file` for a single commit) and gets a new `change-id`; the source commit's id is not copied. When `commit.gpgsign` is enabled, new commits are signed like [`grit commit`](../commit/).

`<commit>` can be a full or short commit id, a branch name (to pick the commit at its tip), or an expression like `feature~2`. Pass several commits to pick them in order.

When a step would conflict, grit stops with conflict markers in the worktree and unmerged index entries — the same model as [`grit revert`](../revert/) and [`grit merge`](../merge/). Resolve conflicts, stage the result with [`grit add`](../add/), then run `grit pick --continue`. Use `--abort` to restore the branch to its pre-pick state, or `--skip` to drop the current step and continue with any remaining commits.

Merge commits require `--mainline <n>` (1-based parent number, long option only).

`grit pick` refuses, and changes nothing, when:

- you have uncommitted changes, or the current branch has no commits, or HEAD is detached
- a commit doesn't change anything, or its change is already on the current branch
- an untracked file would be replaced by a path the pick would check out (same rule as [`grit switch`](../switch/))

## Options

| Option | Description |
| --- | --- |
| `<commit>…` | Commits to pick (required unless continuing). |
| `--mainline <n>` | Parent number when picking a merge commit. |
| `--continue` | Finish a pick stopped on conflicts or an empty commit. |
| `--abort` | Cancel the in-progress pick and restore the saved branch tip. |
| `--skip` | Skip the current commit and continue with the rest of the sequence. |
| `--no-commit` | Apply the pick to the index only; do not create a commit yet. |
| `--empty stop\|keep\|drop` | When a step would not change the tree: stop (default), record an empty commit, or skip the step. |
| `-m`, `--message` | Replace the pick message (**single commit only**). |
| `--message-file` | Read the pick message from a file (**single commit only**). |
| `--dry-run` | List commits that would be picked without changing refs or the worktree. |

## Examples

Pick the latest commit from another branch:

```console
$ grit pick feature
Picked cf18394 → 4be20a1 Say hi
```

Pick a commit by id:

```console
$ grit pick 9a1c2e0
```

When the pick conflicts:

```console
$ grit pick feature
CONFLICT (content): f.txt

Pick stopped on conflicts; fix them and run `grit pick --continue`, or `grit pick --abort` / `grit pick --skip`.
```

## JSON output

Schema: [v1](/schemas/pick/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `completed`, `conflicts`, `aborted`, or `empty`. |
| `source` | string or null | Source commit for the last pick created when `result` is `completed`. |
| `oid` | string or null | New pick commit on the current branch when `result` is `completed`. |
| `subject` | string or null | Subject of the last pick commit when `result` is `completed`. |
| `change_id` | string or null | Stored `change-id` on the last pick commit, or `null`. |
| `commits` | array | Each pick commit created in this invocation (in apply order). |
| `dropped` | array | Source commits skipped as empty per `--empty drop`. |
| `conflicts` | array or null | Conflicted paths when `result` is `conflicts`. |
| `next_steps` | array or null | Suggested follow-up commands when paused. |
| `alternatives` | array or null | `--abort` / `--skip` when paused. |
| `dry_run` | boolean or null | True when `--dry-run` was used. |

Example:

```json
{
  "result": "completed",
  "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "oid": "a8f2c1d0e9b876543210abcdef0123456789abcd",
  "subject": "two",
  "change_id": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
  "commits": [
    {
      "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "oid": "a8f2c1d0e9b876543210abcdef0123456789abcd",
      "subject": "two",
      "change_id": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    }
  ],
  "dropped": []
}
```

When `result` is `conflicts`, exit code is **1** and the object includes `conflicts`, `next_steps`, and `alternatives` (same shape as [`grit merge`](../merge/) conflicts).

## Markdown output

Schema: [v1](/schemas/pick/v1.json)

Pass [`--markdown`](../global-options/) for a `Pick` heading and one bullet per picked commit:

```text
- `cf18394` → `4be20a1` — Say hi
```

## See also

[grit revert](../revert/), [grit merge](../merge/)
