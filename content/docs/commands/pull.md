---
title: grit pull
summary: Fetch from the remote and bring the current branch up to date.
group: Remotes
order: 3
---

## Synopsis

```text
grit pull [--ff-only] [--no-rebase]
```

## Description

Runs [`grit fetch`](../fetch/), then integrates the remote's copy of the current branch into it: a fast-forward when you have no new commits of your own, and a merge commit when both sides diverged (unless `--ff-only` or `pull.ff=only` refuses). On a branch with no commits yet, it adopts the upstream as the first commit.

The remote is the one in `branch.<name>.remote`, or `origin`. The upstream branch comes from [`branch_tracking`](../../library/) (`branch.<name>.merge`), defaulting to the branch with the same name as yours.

Like [`grit merge`](../merge/), `grit pull` won't run with a detached HEAD. Conflicts leave merge-in-progress state with conflict markers in the worktree.

The fetch step uses the same terminal progress as [`grit fetch`](../fetch/) and [`grit clone`](../clone/): live stderr progress on a TTY in human mode, silent when stderr is piped or when you pass `--json` or `--markdown`.

Integration mode follows Git's `pull.ff` and `pull.rebase` config unless overridden on the command line. Rebase pulls (`pull.rebase`) are not supported yet; use `--no-rebase` to merge when `pull.rebase` is set.

## Options

| Flag | Description |
| ---- | ----------- |
| `--ff-only` | Refuse when a merge commit would be required. Exit **1** when diverged; the branch, index, and worktree are unchanged. JSON includes `ahead` and `behind` counts. |
| `--no-rebase` | Merge instead of rebasing, overriding `pull.rebase`. |

See also [global options](../global-options/).

## Exit codes

| Code | When |
| ---- | ---- |
| **0** | Success: up to date, fast-forward, merge, or unborn adoption. |
| **1** | `--ff-only` refused on diverged history (`result`: `refused_not_ff`), or merge conflicts (`result`: `conflicts`). |

## Examples

```console
$ grit pull
Receiving objects: 2.00 KiB, 4.00 KiB/s, done.
Fast-forwarded origin/main → b52cca6

$ grit pull
Merged origin/main into the current branch (acd1d4b)

$ grit pull --ff-only
Not possible to fast-forward origin/main; local is 1 ahead, 2 behind.
```

## JSON output

Schema: [v1](/schemas/pull/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, `refused_not_ff`, `conflicts`, or `set_upstream`. |
| `branch` | string | Remote branch integrated, or local branch name for `set_upstream`. |
| `oid` | string or null | Commit the current branch points at after the pull, when applicable. |
| `change_id` | string or null | Stored `change-id` on `oid` when present. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch adopted. |
| `ahead` | integer or null | Local-only commits when `result` is `refused_not_ff`. |
| `behind` | integer or null | Upstream-only commits when `result` is `refused_not_ff`. |
| `conflicts` | array | Conflicted paths when `result` is `conflicts`; omitted otherwise. |
| `conflict_details` | array or null | Per-path conflict metadata when `result` is `conflicts`. |

Example:

```json
{
  "result": "fast_forward",
  "branch": "origin/main",
  "oid": "b52cca65378cf17309f0b1f917c6dc4fdd9256f2"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/pull/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit fetch](../fetch/), [grit merge](../merge/), [grit push](../push/)
