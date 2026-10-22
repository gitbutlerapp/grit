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
grit rebase|history rebase … [--check] [-X ours|theirs] [--dry-run] [--update-refs=(branches|head)] [--empty=(drop|keep|abort)] [--keep-committer]
```

## Description

`grit rebase` replays commits reachable from `HEAD` but not from `<upstream>` onto `<upstream>` (or onto `--onto` when set). It uses the same history engine as [`grit history`](history.md). When every commit replays cleanly, grit writes new commit objects and updates refs in one batch.

When `<upstream>` is omitted, grit uses `@{upstream}` of the current branch. If no upstream is configured, the command fails with a hint to run [`grit branch -u`](branch.md).

When replay stops on conflicts, grit writes Git-compatible `.git/rebase-merge/` state (including `grit-plan`), leaves conflict markers in the worktree, and exits **1**. The branch tip recorded in `orig-head` is unchanged until you `--continue`, `--skip`, or `--abort`. Resume with the same conflict model as [`grit pick`](pick.md) and [`grit revert`](revert.md): inspect (`grit diff`), resolve, stage (`grit add`), then `grit rebase --continue`, or use `--skip` / `--abort`.

`--check` predicts success or conflicts without touching refs, the index, or the worktree (exit **1** when conflicts are predicted, like [`grit merge --check`](merge.md)). It does not start a rebase session.

`-X ours` and `-X theirs` auto-resolve replay conflicts the way `git rebase -X` does. During replay, **ours** is the onto (upstream) side and **theirs** is the commit being replayed — the opposite of everyday merge wording where “ours” is your checked-out branch.

`--dry-run` still uses the stateless planner only: it lists ref updates without starting a session and without writing objects for a real rebase.

When there is nothing to replay, human output is `Already up to date` and `--json` sets `result` to `completed` with `rewritten` as `[]`.

Shared rewrite flags match [`grit history`](history.md): `--dry-run`, `--update-refs`, `--empty`, and `--keep-committer`.

### Exit codes

| Code | When |
| --- | --- |
| **0** | Rebase completed, `--dry-run` plan printed, `--abort` succeeded, or already up to date. |
| **1** | Replay stopped on conflicts (`result: conflicts` in `--json`). |
| **≠ 0** | Other errors (missing upstream, no session for `--continue`, unmerged index, …). |

## Options

| Option | Description |
| --- | --- |
| `<upstream>` | Exclude commits reachable from this revision (default: `@{upstream}`). |
| `--onto` | Replay onto this revision instead of `<upstream>`. |
| `--continue` | Finish the current commit after resolving conflicts, then replay the rest. |
| `--abort` | Cancel the rebase and restore the branch to `orig-head`. |
| `--skip` | Skip the current commit and replay the rest. |
| `--dry-run` | List ref updates without moving refs (no session). |
| `--update-refs` | `branches` (default) or `head`. |
| `--empty` | `drop` (default), `keep`, or `abort` when a replay would become empty. |
| `--keep-committer` | Preserve committer ident and timestamp on replayed commits. |
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
| `head` | object or null | `{old,new}` when `HEAD`'s commit OID changes. |
| `dry_run` | boolean | Whether refs were left unchanged. |
| `commit` | string | Source commit that stopped replay when `result` is `conflicts`. |
| `conflicts` | array | `{path,kind,summary?}` when `result` is `conflicts`. |
| `next_steps` | array | Ordered `grit` commands after a stop. |
| `alternatives` | array | `grit rebase --abort` and `grit rebase --skip` when paused. |

On conflict:

```json
{
  "result": "conflicts",
  "operation": "rebase",
  "commit": "1a2b3c4d5e6f70819293a4b4c5d6e7f8091a2b3c4d",
  "conflicts": [{ "path": "f.txt", "kind": "content" }],
  "next_steps": ["grit diff", "grit add .", "grit rebase --continue"],
  "alternatives": ["grit rebase --abort", "grit rebase --skip"]
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

## See also

- [`grit history`](history.md) — other rewrite helpers (`reword`, `drop`, `squash`, …)
- [`grit status`](status.md) — `operation` hints while a grit rebase is paused
- [`grit pull --rebase`](pull.md) — fetch then rebase onto the remote tip
- [`grit push --force-with-lease`](push.md) — publish rewritten history safely
