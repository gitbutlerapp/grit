---
title: grit pick
summary: Cherry-pick one or more commits onto the current branch.
group: Branches and tags
order: 4
---

## Synopsis

```text
grit pick [<commit> | <range>]...
grit pick --continue | --abort | --skip
```

## Description

Replays commits from another branch (or arbitrary revisions) onto the current branch, like `git cherry-pick`. Each new commit keeps the original **author** and message (unless you pass `--message` / `--message-file` for a single commit), gets a fresh **`change-id`**, and records a normal reflog entry.

Positional arguments are applied **oldest first**. Each argument is either:

- a single commit (`feature`, `abc1234`, `HEAD~2`), or
- a two-dot range `A..B` (commits reachable from `B` but not from `A`, same as `git log A..B`).

`grit pick` stops — leaving Git-compatible sequencer state — when:

- a step has **merge conflicts** (conflict markers in the worktree, `CHERRY_PICK_HEAD`, `MERGE_MSG`, `.git/sequencer/`)
- a step would be **empty** and `--empty stop` (default)
- the worktree is **dirty**, `HEAD` is **detached**, or the branch has **no commits**
- a commit is a **merge** without `--mainline` (not yet exposed on the CLI)

Resolve conflicts, stage with [`grit add`](../add/), then run `grit pick --continue`. Use `grit pick --skip` to drop the current step, or `grit pick --abort` to restore the branch to its pre-pick tip.

## Options

| Option | Description |
| --- | --- |
| `<commit>`… | Commits or ranges to replay (oldest first). |
| `--continue` | Finish the paused pick after conflicts are resolved and staged. |
| `--abort` | Cancel the in-progress pick and restore the saved branch tip. |
| `--skip` | Skip the current todo step and continue with the rest. |
| `-x`, `--record-origin` | Append `(cherry picked from commit …)` to each new message. |
| `--no-commit` | Apply the patch to the index and worktree without committing. |
| `--empty` | When a step would not change the tree: `stop` (default), `keep` (empty commit), or `drop`. |
| `-m`, `--message` | Replace the message (only with exactly one commit). |
| `--message-file` | Read the message from a file (only with exactly one commit). |

## Examples

Pick the latest commit on a topic branch:

```console
$ grit pick feature
Picked cf18394 → 4be20a1 Say hi
```

Pick a range of commits:

```console
$ grit pick main..feature
Picked a1b2c3d → d4e5f6a First change
Picked b2c3d4e → e5f6a7b Second change
```

Conflict, then continue:

```console
$ grit pick topic~1..topic
CONFLICT (content): shared.txt
→ grit restore --ours shared.txt (or edit markers)
→ grit add shared.txt
→ grit pick --continue
Picked … → … Resolved topic change
```

## JSON output

Schema: [v2](/schemas/pick/v2.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `picked`, `conflicts`, `empty`, `aborted`, or `skipped`. |
| `commits` | array | Each picked or attempted step: `source`, `oid`, `change_id`, `subject`. |
| `dropped` | array | Source commit oids skipped as empty (`--empty drop`). |
| `conflicts` | array or null | Conflicted paths when `result` is `conflicts`. |
| `next_steps` | array or null | Suggested commands when stopped (`conflicts` or `empty`). |
| `alternatives` | array or null | `grit pick --abort` / `--skip` when a sequence is paused. |

Example (success):

```json
{
  "result": "picked",
  "commits": [
    {
      "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
      "change_id": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
      "subject": "Say hi"
    }
  ],
  "dropped": []
}
```

On conflicts, exit code is **1** with `result: "conflicts"` and the same conflict shape as [`grit merge`](../merge/).

## Markdown output

Schema: [v2](/schemas/pick/v2.json)

Pass [`--markdown`](../global-options/) for a `Pick` heading, one bullet per replayed commit, and conflict next steps when stopped.

## Performance

The **`pick-range-20`** grit-bench scenario (`grit pick base..topic` vs `git cherry-pick` on a 20-commit topic in the **synthetic-10000** fixture) tracks multi-commit replay on the daily matrix. The committed baseline (**`grit-utils/baselines/hot-paths-after.json`**) records a **known gap** of about **2.0×** Git median wall time and about **1.6×** peak RSS on the factory VM (release hyperfine, system Git **2.43**). The acceptance **goal** is unchanged: **≤ 1.0×** time and **≤ 1.2×** RSS. Closing the gap is tracked as follow-up work on **[GitHub issue #2977](https://github.com/gitbutlerapp/grit/issues/2977)** (shared replay merge/checkout path: index-hinted tree diffs, scoped checkout, lower peak RSS — not pick-only shortcuts). See [Benchmarks](../benchmarks.md#pick-range-20-known-gap).

## See also

[grit merge](../merge/), [grit revert](../revert/), [grit log](../log/)
