---
title: grit pick
summary: Cherry-pick one or more commits onto the current branch.
group: Branches and tags
order: 4
---

## Synopsis

```text
grit pick [<commit> | <range>]...
grit pick --check [<commit> | <range>]...
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

Resolve conflicts, then either run [`grit commit`](../commit/) (auto-stages the worktree and finishes the step, keeping the picked author) or stage with [`grit add`](../add/) and run `grit pick --continue`. Use `grit pick --skip` to drop the current step, or `grit pick --abort` to cancel. If the branch tip moved because you already committed the resolution, `--abort` clears replay state without moving the branch (Git warns and exits 0 in the same situation).

Pick either completes a step, stops for conflicts or an empty commit, or changes nothing. Pressing Ctrl+C, or sending `SIGTERM` or `SIGHUP` on Unix, while files are being checked out stops the pick with `error: operation cancelled`; `HEAD`, the index and the work tree are left as they were before the pick started (no `CHERRY_PICK_HEAD`), no `.git/index.lock` is left behind, and the same pick can simply be run again. This works the same in a Windows console, where Ctrl+Break counts as Ctrl+C. A second interrupt terminates grit at once, without that cleanup; only `SIGKILL` skips rollback.

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
| `-F`, `--message-file` | Read the message from a file, or `-` for stdin (only with exactly one commit). |
| `--mainline` | Parent number when replaying a merge commit (library support; CLI wiring pending). |
| `--check` | Predict whether the pick would succeed without changing refs, the index, or the worktree. Exit **0** when every step would apply cleanly; exit **1** when a step would conflict or stop as empty (with `--empty stop`). Conflicts with `--continue`, `--abort`, `--skip`, and `--dry-run`. |
| `--dry-run` | Print the commits that would be replayed without changing refs or the worktree. Conflicts with `--check`. |
| `-X ours\|theirs` | Auto-resolve replay conflicts: keep the current branch side (`ours`) or the picked change (`theirs`). |
| `--conflict merge\|diff3\|zdiff3` | Conflict marker style when a step stops with conflicts (default from `merge.conflictStyle`). |

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

While a rebase is paused, `grit pick` refuses (it does not treat conflict markers as a dirty worktree):

```console
$ grit pick main
error: cannot pick while a rebase is in progress; finish it with `grit rebase --continue` or cancel it with `grit rebase --abort`
```

Conflict, then continue:

```console
$ grit pick topic~1..topic
CONFLICT (both_modified): shared.txt
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
| `result` | string | `picked`, `clean`, `conflicts`, `empty`, `aborted`, or `skipped`. |
| `oid` | string or null | Predicted final tree oid when `result` is `clean` (`--check` only). |
| `check` | boolean | True when `--check` was used (nothing was picked). |
| `commits` | array | Each picked or attempted step: `source`, `oid`, `change_id`, `subject`. |
| `dropped` | array | Source commit oids skipped as empty (`--empty drop`). |
| `conflicts` | array or null | Conflicted paths when `result` is `conflicts`, each `{path, kind}` with a [conflict kind](../status/#conflict-kinds). |
| `next_steps` | array or null | Suggested commands when stopped (`conflicts` or `empty`). |
| `alternatives` | array or null | `grit pick --abort` / `--skip` when a sequence is paused. |
| `dry_run` | boolean or null | `true` when `--dry-run` was set (no refs or objects were updated). |

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




```text
## Pick

- `d5d832b` → `d5d832b` — pick me
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Pick completed or `--check` predicts a clean replay. |
| `1` | Conflicts, empty commit, or other failure. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit merge](../merge/), [grit revert](../revert/), [grit log](../log/)