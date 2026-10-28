---
title: grit rebase
summary: Replay local commits onto a new base; recover from conflicts and exec commands that leave edits.
group: History
order: 5
---

## Synopsis

```text
grit rebase [<upstream>]
grit rebase [--onto <rev>] [<upstream>]
grit rebase --continue | --abort | --skip
grit history rebase [<upstream>]
grit rebase|history rebase … [--check] [-X ours|theirs] [--dry-run] [--exec <cmd>]… [--update-refs=(branches|head)] [--empty=(drop|keep|abort)] [--keep-committer] [--no-verify]
```

## Description

`grit rebase` replays commits reachable from `HEAD` but not from `<upstream>` onto `<upstream>` (or onto `--onto` when set). It uses the same history engine as [`grit history`](history.md). When every commit replays cleanly, grit writes new commit objects and updates refs in one batch.

Before it replays anything, grit runs the `pre-rebase` hook with the upstream as you typed it (`pre-rebase main`), or the configured upstream's full ref name (`pre-rebase refs/remotes/origin/main`) when you omitted it, as Git does. It runs once there is something to replay and before any commit is replayed, so a refusal comes before a replay conflict on every path (`-X` and `--exec` included): not when the branch is already up to date, and not for `--check`, `--dry-run`, `--continue`, `--skip` or `--abort`. When the hook exits non-zero, the rebase stops with exit **1** and a `hook_failed` error, and nothing changes. `--no-verify` skips it.

```console
$ grit rebase main
error: hook pre-rebase failed with exit status 1
$ grit rebase main --no-verify
Rebased 1 commit(s)
  commit 9c1f2d3 → 4e5a6b7 (feature work)
  refs/heads/feature 9c1f2d3 → 4e5a6b7
```

When the rebase moves the branch, grit runs the `post-checkout` hook with Git's arguments, `<old tip> <onto> 1`: Git checks out the onto commit before replaying, and the hook names that commit even though grit applies the rebase in one step. This also happens when a rebase stops on conflicts. When the rebase finishes, grit runs `post-rewrite rebase` with one `<old> <new>` line per replayed commit on stdin, oldest first. Dropped commits, commits already upstream, and fast-forwards are not listed, so a fast-forward rebase runs only `post-checkout`. Unlike Git, grit does not run `post-commit` for each replayed commit. Hook exit statuses never change the result. See [`grit hooks`](hooks.md).

A commit whose patch is already upstream (for example one you cherry-picked from the upstream branch) is skipped, as `git rebase` skips it. The commits before and after it are still replayed in order on top of the new base, and a branch that pointed at the skipped commit moves to the replayed commit before it, or to the new base when nothing came before. In the output and the `rewritten` JSON rows, the skipped commit maps to the upstream commit that carries its patch.

When `<upstream>` is omitted, grit uses `@{upstream}` of the current branch. If no upstream is configured, the command fails with a hint to run [`grit branch -u`](branch.md).

When replay stops on conflicts, grit writes Git-compatible `.git/rebase-merge/` state (including `grit-plan`), leaves conflict markers in the worktree, and exits **1**. As with [`grit pick`](pick.md) (and `git rebase`), the incoming `>>>>>>>` marker names the commit being replayed as `<abbrev> (<subject>)`, so each stop shows which commit it came from; the current side is `HEAD`. The branch tip recorded in `orig-head` is unchanged until you `--continue`, `--skip`, or `--abort`. Resume with the same conflict model as [`grit pick`](pick.md) and [`grit revert`](revert.md): inspect (`grit diff`), resolve, stage (`grit add`), then `grit rebase --continue`, or use `--skip` / `--abort`. [`grit pull --rebase`](pull.md) stops the same way.

When the replayed range contains merge commits (the default, unlike `git pull --rebase`), grit keeps merge topology like `git rebase --rebase-merges`: the first-parent chain from your branch tip is replayed onto the new base, side branches merged in stay on their own parentage until mapping moves them, and resolving a conflict on a merge commit with `--continue` keeps every parent (the current `HEAD` as the first parent, mapped originals for the rest). Merged-in files and refs on side branches are not dropped silently.

Stopping resets the index and worktree to the replayed commits, so grit only stops when that loses nothing. If replay would conflict while you have staged or unstaged changes, or an untracked file sits where the stop would check out a path, the rebase refuses (`kind: dirty_worktree`, or the untracked-overwrite error) and nothing changes; commit or stash first. A rebase that replays cleanly still carries local changes along, like a branch switch.

The conflict stop state is complete enough for `git` to take over: `git rebase --continue`, `--skip` and `--abort` all work on a rebase grit started (grit writes `author-script`, `stopped-sha` and the current pick in `done`, as git does). With the default `--update-refs=branches`, Git's todo lists only the rebased branch's own commits so `git rebase --continue` finishes that branch; stacked-branch updates happen when you resume with `grit rebase --continue`.

A rebase that replays commits records the branch's pre-rebase tip in `ORIG_HEAD`, as `git rebase` does. That includes `-X ours|theirs`, `--exec`, a rebase that stops on conflicts, and an `--exec` stop (`exec_dirty` or a replay conflict after an amending command); an `--exec` command that fails rolls everything back and leaves `ORIG_HEAD` alone. It stays there after the rebase finishes, `--continue`, `--skip` or `--abort`, so `grit reset --hard ORIG_HEAD` undoes a finished rebase. When there is nothing to replay (`Already up to date`), `--check` and `--dry-run`, `ORIG_HEAD` is left alone.

A rebase that `git` started (`.git/rebase-merge/` or `.git/rebase-apply/` without `grit-plan`) blocks `grit rebase <upstream>` and [`grit switch`](switch.md). `grit rebase --abort` cancels it: `HEAD` is re-attached to the rebased branch at `orig-head`, the index and worktree are reset, git's state directory is removed, and unresolved rerere records for the abandoned conflict (`MERGE_RR`) are cleared, as `git rebase --abort` does. When git recorded an autostash, grit leaves the rebase alone and asks you to run `git rebase --abort`. `grit rebase --continue` and `--skip` refuse with a pointer to `git rebase --continue` / `--skip`, since only git can resume its own todo list.

`--check` predicts success or conflicts without touching refs, the index, or the worktree (exit **1** when conflicts are predicted, like [`grit merge --check`](merge.md)). It does not start a rebase session.

`-X ours` and `-X theirs` auto-resolve replay conflicts the way `git rebase -X` does. During replay, **ours** is the onto (upstream) side and **theirs** is the commit being replayed — the opposite of everyday merge wording where “ours” is your checked-out branch. A conflict that `-X` cannot resolve (for example a file deleted on one side and modified on the other) stops the rebase like any other conflict, and `--continue` keeps replaying with the same `-X` side.

`--dry-run` still uses the stateless planner only: it lists ref updates without starting a session and without writing objects for a real rebase.

When there is nothing to replay, human output is `Already up to date` and `--json` sets `result` to `completed` with `rewritten` as `[]`.

Shared rewrite flags match [`grit history`](history.md): `--dry-run`, `--update-refs`, `--empty`, and `--keep-committer`.

With the default `--update-refs branches`, stacked branches move with the rebase: a branch on one of the replayed commits, or on a commit built on top of them, ends up on its replayed copy. Another branch at the same commit as the checked-out branch is not moved, so a `backup` branch created right before the rebase still points at the original commits:

```console
$ grit branch backup
$ grit rebase main
Rebased 2 commit(s)
  commit cc10d19 → 931f74a (X)
  commit 874d334 → 8489106 (Y)
  refs/heads/feature 874d334 → 8489106
$ grit log --oneline backup
874d334 Y
cc10d19 X
70709d8 base
```

### Exec commands that amend or add commits

An `--exec` command may rewrite the commit it runs at, the way it can with `git rebase
--exec`. When it exits zero with a detached `HEAD` that amends the replay commit, or
adds new commits on top of it, grit keeps the result: later commits are replayed onto
the new `HEAD`, later commands see its tree, and the branch ends up on the rewritten
history. The common "format every commit" loop works as expected:

```console
$ grit rebase main --exec 'cargo fmt && git commit -qa --amend --no-edit'
Rebased 2 commit(s)
  commit d9b09e2 → 4ec1aa7 (one)
  commit 3fe1bb2 → 0c24bc2 (two)
  refs/heads/feature 3fe1bb2 → 0c24bc2
```

`git commit --amend` keeps the commit's change id. A command that leaves `HEAD`
anywhere else (switching branches, resetting the commit away) fails the rebase: grit
restores `HEAD`, the index, and the worktree without moving refs, and the commits the
command made stay in the object database.

If a later commit no longer applies on top of an amended one, replay stops with
`result: "conflicts"` and conflict markers, like any rebase conflict, without
undoing the commits already rewritten. Resolve and stage the paths, then run
`grit rebase --continue` (the exec commands run at the resolved commit), or use
`--skip` to drop that commit (its exec commands still run, at the current `HEAD`, as
with Git), or `--abort` to restore the original branch. Resuming keeps the options the
rebase started with, such as `--empty keep`; there is no need to repeat them.

### Successful exec commands that leave edits

After **every** successful `--exec` command, grit reloads the index and checks tracked
staged and unstaged changes against the resulting `HEAD` (the replay commit, or the
commit the command amended it into). If anything changed, replay stops immediately
with exit **1** and `result: "exec_dirty"`. The edits and index stay exactly as the
command left them, `HEAD` remains detached at that commit, and the original branch
refs remain unchanged. Later exec commands do not run.

Save the edits before continuing:

```bash
grit stash push -m "exec edits"
grit rebase --continue
grit stash pop
```

`--continue` resumes after the successful command, so it does not repeat the edit.
`--skip` skips any remaining exec commands at that commit and keeps the already
replayed commit. Both require a clean index and worktree at the recorded replay
HEAD. If you move HEAD or create a commit while stopped, save new commits on a
branch, then abort and restart. `--abort` explicitly discards exec edits and restores
the original HEAD, index, and tracked files; save any edits you want first.

Exec stops use grit's `rebase-merge/grit-exec` recovery record. Resume these with
grit. Commands exiting nonzero retain their existing rollback behavior and command
exit code.

If grit is interrupted (Ctrl-C or `kill`) while an `--exec` command is running, the
recovery record is kept at the detached replay commit. `grit status` reports the
interrupted rebase and suggests `--continue` or `--abort`; `--abort` restores the
branch tip, index, and worktree from before the rebase started.

### When the branch moves during a stop

Something else may move the rebased branch while the rebase is stopped: a commit from
another worktree, `git branch -f`, `git update-ref`, or checking the branch out and
amending it. The saved plan no longer applies then, so `--continue` and `--skip` refuse
with exit **1** and change nothing:

```console
$ grit rebase --continue
error: refs/heads/feature moved to 045e4cc while the rebase was stopped, so it cannot be continued or skipped; run `grit rebase --abort` to restore refs/heads/feature to b37ca7f (the moved tip stays in its reflog)
```

`--abort` always works: it resets the branch to its pre-rebase tip (`orig-head`), clears
the rebase state, and reports the tip it overwrote, which stays reachable as
`<branch>@{1}` (logged as `rebase (abort)`). This holds for conflict stops and exec stops
alike:

```console
$ grit rebase --abort
Rebase aborted.
  refs/heads/feature was moved during the rebase; reset 045e4cc → b37ca7f (the moved tip stays in its reflog)
```

In `--json`, the overwritten branch appears in `refs` as `{ref, old, new}`, with `old`
the moved tip and `new` the restored one. An abort of an unmoved branch has no `refs`.

## Options

| Option | Description |
| --- | --- |
| `<upstream>` | Exclude commits reachable from this revision (default: `@{upstream}`). |
| `--onto` | Replay onto this revision instead of `<upstream>`. |
| `--continue` | Finish the current conflict resolution (including a replay conflict after an amending exec), or resume exec after saving its edits. |
| `--abort` | Cancel the rebase and restore the branch to `orig-head`, even if something moved it during the stop (the overwritten tip is reported). Also cancels a rebase that `git` started. |
| `--skip` | Skip a conflicted commit, or remaining exec commands at an exec stop (save edits first). |
| `--dry-run` | List ref updates without moving refs (no session). |
| `--update-refs` | `branches` (default: the checked-out branch and every stacked branch on or above the replayed commits, but not other branches at the `HEAD` commit) or `head`. |
| `--empty` | `drop` (default), `keep`, or `abort` when a replay would become empty. |
| `--keep-committer` | Preserve committer ident and timestamp on replayed commits. |
| `--exec` | After each replayed commit is checked out, run `CMD` via `sh -c` in the worktree (repeatable; runs in order at every commit). Requires a clean index and worktree. Incompatible with `--dry-run`, `--check`, and `-X`. Command output goes to stderr. On nonzero command exit, grit restores `HEAD`, the index, and the worktree without moving refs; exit code is the command's exit code. A successful command may amend the replayed commit or add commits on top of it; later commits are replayed onto the result. A successful command that leaves tracked edits stops with `exec_dirty` and preserves them for recovery. |
| `--check` | Predict success or conflicts without changing refs, index, or worktree (conflicts with `--dry-run`). |
| `-X ours\|theirs` | Auto-resolve replay conflicts in favor of the onto side (`ours`) or replayed commits (`theirs`). |
| `--no-verify` | Skip the `pre-rebase` hook. |

## Examples

```console
$ grit rebase --onto release/v2 origin/main --dry-run
Dry run — refs would change:
  commit abc1234 → def5678 (feature work)
  refs/heads/feature abc1234 → def5678
```

```console
$ grit rebase origin/main
CONFLICT (both_modified): docs/guide.md

Rebase stopped on conflicts; fix them and run `grit rebase --continue`, or `grit rebase --abort` / `grit rebase --skip`.
$ grit add docs/guide.md
$ grit rebase --continue
Rebased 3 commit(s)
```

## JSON output

Schema: [v1](/schemas/rebase/v1.json)

Pass `--json` on `grit rebase` or `grit history rebase`:

| Field | Type | Meaning |
| --- | --- | --- |
| `result` | string | `completed`, `conflicts`, `exec_dirty`, or `aborted`. |
| `operation` | string | `rebase` when present. |
| `rewritten` | array | `{old,new,change_id,subject}` per replayed commit when `result` is `completed`. |
| `refs` | array | `{ref,old,new}` for each ref that moved. For `aborted`, each branch the abort reset after something else moved it during the stop (`old` is the overwritten tip, `new` the restored `orig-head`); omitted when nothing was overwritten. |
| `head` | object or null | `{old,new}` for the completed invocation's `HEAD` OID change; `null` when unchanged. Dry runs show the planned change; conflict and abort outcomes use `null`. |
| `dry_run` | boolean | Whether refs were left unchanged. |
| `exec` | array | Captured invocations during this call, on completion or an exec stop: `{commit, command, exit_code}` per invocation (full hex `commit`). |
| `commit` | string | Source commit for `conflicts`; `HEAD` after the command (the replay commit or its amended replacement) for `exec_dirty`. |
| `command` | string | Successful command that left edits, for `exec_dirty`. |
| `paths` | array of strings | Changed tracked paths at an exec stop. |
| `conflicts` | array | `{path,kind,summary?}` when `result` is `conflicts`; `kind` is a [conflict kind](../status/#conflict-kinds). |
| `next_steps` | array | Ordered `grit` commands after a stop. |
| `alternatives` | array | `grit rebase --abort` and `grit rebase --skip` when paused. |

For completed results, `head.old` is the OID immediately before this command and `head.new` is the final OID. On `--continue` and `--skip`, this compares the stopped, detached `HEAD` with the completed branch tip after reattachment. `refs` and `rewritten` still describe the whole rebase, so their old OIDs can differ from `head.old`. Skipping the final conflicting commit can reattach `HEAD` without changing its OID; then `head` is `null`.

After resolving and staging a conflict, `grit rebase --continue --json` reports:

```json
{
  "result": "completed",
  "operation": "rebase",
  "rewritten": [
    {
      "old": "420bacbf7a90d30965b669336667fc144be897c1",
      "new": "77fc7445860c1cc5a111ddd9bc9e0be97884522c",
      "change_id": "rwlqysmxxrwkltttnnqttmptqznozqul",
      "subject": "feature"
    }
  ],
  "refs": [
    {
      "ref": "refs/heads/feature",
      "name": "refs/heads/feature",
      "old": "420bacbf7a90d30965b669336667fc144be897c1",
      "new": "77fc7445860c1cc5a111ddd9bc9e0be97884522c"
    }
  ],
  "head": {
    "old": "8f302b0a5bc73aecb1c2f23b8a00953c7c64ed6c",
    "new": "77fc7445860c1cc5a111ddd9bc9e0be97884522c"
  }
}
```

With `--check` when already up to date:

```json
{
  "operation": "rebase",
  "result": "up_to_date",
  "check": true
}
```

With `--check --json`, stdout is a result object instead of a stop/resume payload:

Schema: [rebase.check v1](/schemas/rebase.check/v1.json)

| Field | Type | Meaning |
| --- | --- | --- |
| `operation` | string | Always `rebase`. |
| `result` | string | `up_to_date`, `clean`, or `conflicts`. |
| `check` | boolean | Always `true`. |
| `commits` | array of strings | Replacement commit OIDs when `result` is `clean`. |
| `conflicts` | array | `{path, kind}` entries when `result` is `conflicts`, with the [conflict kind](../status/#conflict-kinds) the replay would leave. |
| `commit` | string | Original commit OID when `result` is `conflicts`. |
| `refs` | array of strings | Present when the conflicting commit is only on other branches that `--update-refs branches` would move: their full names. Rebasing with `--update-refs head` leaves them alone. |

Exit code is **0** for `up_to_date` and `clean`, **1** for `conflicts`.

## Markdown output

Schema: [v1](/schemas/rebase/v1.json)

Pass [`--markdown`](../global-options/) for a `## Rebase` heading (or `## History rebase` for the alias). On conflict or exec stops, lists paths, the stopped commit, and `next_steps` / `alternatives` like pick and revert.



```text
## Rebase check

- Already up to date
```

On a branch with no commits yet, rebase onto any upstream fails with `branch_no_commits` and a hint to make the first commit (see [`grit log`](log.md)).

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Rebase completed, `--check` predicts success, or `--dry-run`. |
| `1` | Conflicts, successful exec with tracked edits, `pre-rebase` refused (`kind: hook_failed`), or other failure. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| *varies* | `--exec` command failure ([`custom`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

- [`grit history`](history.md) — other rewrite helpers (`reword`, `drop`, `squash`, …)
- [`grit status`](status.md) — `operation` hints while a grit rebase is paused
- [`grit pull --rebase`](pull.md) — fetch then rebase onto the remote tip
- [`grit push --force-with-lease`](push.md) — publish rewritten history safely
