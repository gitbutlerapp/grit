---
title: grit rebase
summary: Replay local commits onto a new base; stop on conflicts and resume with --continue.
group: History
order: 5
---

## Synopsis

```text
grit rebase [<upstream>]
grit rebase [--onto <rev>] [<upstream>]
grit rebase --continue | --abort | --skip
grit history rebase [<upstream>]
grit rebase|history rebase … [--check] [-X ours|theirs] [--dry-run] [--exec <cmd>]… [--update-refs=(branches|head)] [--empty=(drop|keep|abort)] [--keep-committer]
```

## Description

`grit rebase` replays commits reachable from `HEAD` but not from `<upstream>` onto `<upstream>` (or onto `--onto` when set). It uses the same history engine as [`grit history`](history.md). When every commit replays cleanly, grit writes new commit objects and updates refs in one batch.

When the rebase moves the branch, grit runs the `post-checkout` hook with Git's arguments, `<old tip> <onto> 1`: Git checks out the onto commit before replaying, and the hook names that commit even though grit applies the rebase in one step. This also happens when a rebase stops on conflicts. When the rebase finishes, grit runs `post-rewrite rebase` with one `<old> <new>` line per replayed commit on stdin, oldest first. Dropped commits, commits already upstream, and fast-forwards are not listed, so a fast-forward rebase runs only `post-checkout`. Unlike Git, grit does not run `post-commit` for each replayed commit. Hook exit statuses never change the result. See [`grit hooks`](hooks.md).

When `<upstream>` is omitted, grit uses `@{upstream}` of the current branch. If no upstream is configured, the command fails with a hint to run [`grit branch -u`](branch.md).

When replay stops on conflicts, grit writes Git-compatible `.git/rebase-merge/` state (including `grit-plan`), leaves conflict markers in the worktree, and exits **1**. The branch tip recorded in `orig-head` is unchanged until you `--continue`, `--skip`, or `--abort`. Resume with the same conflict model as [`grit pick`](pick.md) and [`grit revert`](revert.md): inspect (`grit diff`), resolve, stage (`grit add`), then `grit rebase --continue`, or use `--skip` / `--abort`. [`grit pull --rebase`](pull.md) stops the same way.

Stopping resets the index and worktree to the replayed commits, so grit only stops when that loses nothing. If replay would conflict while you have staged or unstaged changes, or an untracked file sits where the stop would check out a path, the rebase refuses (`kind: dirty_worktree`, or the untracked-overwrite error) and nothing changes; commit or stash first. A rebase that replays cleanly still carries local changes along, like a branch switch.

The stopped state is complete enough for `git` to take over: `git rebase --continue`, `--skip` and `--abort` all work on a rebase grit started (grit writes `author-script`, `stopped-sha` and the current pick in `done`, as git does).

A rebase that `git` started (`.git/rebase-merge/` or `.git/rebase-apply/` without `grit-plan`) blocks `grit rebase <upstream>` and [`grit switch`](switch.md). `grit rebase --abort` cancels it: `HEAD` is re-attached to the rebased branch at `orig-head`, the index and worktree are reset, git's state directory is removed, and unresolved rerere records for the abandoned conflict (`MERGE_RR`) are cleared, as `git rebase --abort` does. When git recorded an autostash, grit leaves the rebase alone and asks you to run `git rebase --abort`. `grit rebase --continue` and `--skip` refuse with a pointer to `git rebase --continue` / `--skip`, since only git can resume its own todo list.

`--check` predicts success or conflicts without touching refs, the index, or the worktree (exit **1** when conflicts are predicted, like [`grit merge --check`](merge.md)). It does not start a rebase session.

`-X ours` and `-X theirs` auto-resolve replay conflicts the way `git rebase -X` does. During replay, **ours** is the onto (upstream) side and **theirs** is the commit being replayed — the opposite of everyday merge wording where “ours” is your checked-out branch.

`--dry-run` still uses the stateless planner only: it lists ref updates without starting a session and without writing objects for a real rebase.

When there is nothing to replay, human output is `Already up to date` and `--json` sets `result` to `completed` with `rewritten` as `[]`.

Shared rewrite flags match [`grit history`](history.md): `--dry-run`, `--update-refs`, `--empty`, and `--keep-committer`.

## Options

| Option | Description |
| --- | --- |
| `<upstream>` | Exclude commits reachable from this revision (default: `@{upstream}`). |
| `--onto` | Replay onto this revision instead of `<upstream>`. |
| `--continue` | Finish the current commit after resolving conflicts, then replay the rest. |
| `--abort` | Cancel the rebase and restore the branch to `orig-head`. Also cancels a rebase that `git` started. |
| `--skip` | Skip the current commit and replay the rest. |
| `--dry-run` | List ref updates without moving refs (no session). |
| `--update-refs` | `branches` (default) or `head`. |
| `--empty` | `drop` (default), `keep`, or `abort` when a replay would become empty. |
| `--keep-committer` |
| `--exec` | After each replayed commit is checked out, run `CMD` via `sh -c` in the worktree (repeatable; runs in order at every commit). Requires a clean index and worktree. Incompatible with `--dry-run`, `--check`, and `-X`. Command output goes to stderr. On failure grit restores `HEAD`, the index, and the worktree without moving refs; exit code is the command's exit code. |
 Preserve committer ident and timestamp on replayed commits. |
| `--check` | Predict success or conflicts without changing refs, index, or worktree (conflicts with `--dry-run`). |
| `-X ours\|theirs` | Auto-resolve replay conflicts in favor of the onto side (`ours`) or replayed commits (`theirs`). |

## Examples

```console
$ grit rebase --onto release/v2 origin/main --dry-run
Dry run — refs would change:
  commit abc1234 → def5678 (feature work)
  refs/heads/feature abc1234 → def5678
```

```console
$ grit rebase origin/main
CONFLICT (content): docs/guide.md

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
| `result` | string | `completed`, `conflicts`, or `aborted`. |
| `operation` | string | `rebase` when present. |
| `rewritten` | array | `{old,new,change_id,subject}` per replayed commit when `result` is `completed`. |
| `refs` | array | `{ref,old,new}` for each ref that moved. |
| `head` | object or null | `{old,new}` for the completed invocation's `HEAD` OID change; `null` when unchanged. Dry runs show the planned change; conflict and abort outcomes use `null`. |
| `dry_run` | boolean | Whether refs were left unchanged. |
| `exec` | array | When `--exec` was used and the rebase succeeded: `{commit, command, exit_code}` per invocation (full hex `commit`). |
| `commit` | string | Source commit that stopped replay when `result` is `conflicts`. |
| `conflicts` | array | `{path,kind,summary?}` when `result` is `conflicts`. |
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
| `conflicts` | array | `{path, kind}` entries when `result` is `conflicts`. |
| `commit` | string | Original commit OID when `result` is `conflicts`. |

Exit code is **0** for `up_to_date` and `clean`, **1** for `conflicts`.

## Markdown output

Schema: [v1](/schemas/rebase/v1.json)

Pass [`--markdown`](../global-options/) for a `## Rebase` heading (or `## History rebase` for the alias). On conflicts, lists paths, the stopped commit, and `next_steps` / `alternatives` like pick and revert.



```text
## Rebase check

- Already up to date
```

On a branch with no commits yet, rebase onto any upstream fails with `branch_no_commits` and a hint to make the first commit (see [`grit log`](log.md)).

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Rebase completed, `--check` predicts success, or `--dry-run`. |
| `1` | Conflicts or other failure. |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| *varies* | `--exec` command failure ([`custom`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

- [`grit history`](history.md) — other rewrite helpers (`reword`, `drop`, `squash`, …)
- [`grit status`](status.md) — `operation` hints while a grit rebase is paused
- [`grit pull --rebase`](pull.md) — fetch then rebase onto the remote tip
- [`grit push --force-with-lease`](push.md) — publish rewritten history safely
