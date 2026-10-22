---
title: grit rebase
summary: Replay local commits onto a new base with atomic ref updates.
group: History
order: 5
---

## Synopsis

```text
grit rebase [<upstream>]
grit rebase [--onto <rev>] [<upstream>]
grit history rebase [<upstream>]
grit rebase|history rebase … [--check] [-X ours|theirs] [--dry-run] [--update-refs=(branches|head)] [--empty=(drop|keep|abort)] [--keep-committer]
```

## Description

`grit rebase` replays commits reachable from `HEAD` but not from `<upstream>` onto `<upstream>` (or onto `--onto` when set). It uses the same stateless history engine as [`grit history`](history.md): grit plans the rewrite, writes new commit objects, then updates refs in one batch. There is no sequencer directory and **no Git hooks** during planning or apply.

When `<upstream>` is omitted, grit uses `@{upstream}` of the current branch. If no upstream is configured, the command fails with a hint to run [`grit branch -u`](branch.md).

Conflicts during planning abort atomically: exit **1**, structured JSON with `kind: "conflict"`, conflicting commit and paths, and **no** change to refs, the index, or the worktree. `--check` predicts the same outcome without touching refs, the index, or the worktree (exit **1** when conflicts are predicted, like [`grit merge --check`](merge.md)). Stop/continue rebase is planned separately.

`-X ours` and `-X theirs` auto-resolve replay conflicts the way `git rebase -X` does. During replay, **ours** is the onto (upstream) side and **theirs** is the commit being replayed — the opposite of everyday merge wording where “ours” is your checked-out branch.

When there is nothing to replay, human output is `Already up to date` and `--json` sets `rewritten` to `[]`.

Shared rewrite flags match [`grit history`](history.md): `--dry-run`, `--update-refs`, `--empty`, and `--keep-committer`.

## Options

| Option | Description |
| --- | --- |
| `<upstream>` | Exclude commits reachable from this revision (default: `@{upstream}`). |
| `--onto` | Replay onto this revision instead of `<upstream>`. |
| `--dry-run` | List ref updates without moving refs. |
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

## JSON output

Schema: [v1](/schemas/rebase/v1.json)

Pass `--json` on `grit rebase` or `grit history rebase`:

| Field | Type | Meaning |
| --- | --- | --- |
| `operation` | string | Always `rebase`. |
| `rewritten` | array | `{old,new,change_id,subject}` full hex OIDs per replayed commit. |
| `refs` | array | `{ref,old,new}` for each ref that would move or moved. `name` duplicates `ref` and is deprecated. |
| `head` | object or null | `{old,new}` when `HEAD`'s commit OID changes; `null` when unchanged. |
| `dry_run` | boolean | Whether refs were left unchanged. |

Example:

```json
{
  "operation": "rebase",
  "rewritten": [
    {
      "old": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "new": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "change_id": null,
      "subject": "second"
    },
    {
      "old": "1a2b3c4d5e6f70819293a4b4c5d6e7f8091a2b3c4d",
      "new": "1a2b3c4d5e6f70819293a4b4c5d6e7f8091a2b3c4d",
      "change_id": null,
      "subject": "third"
    }
  ],
  "refs": [],
  "head": null,
  "dry_run": true
}
```

On conflict, stdout is a structured error (`kind: "conflict"`, `commit`, `conflicts`) and exit code is **1**.

With `--check --json`, stdout is a result object instead of an error:

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

Pass [`--markdown`](../global-options/) for a `## Rebase` heading (or `## History rebase` for the alias), rewritten commits, ref updates, and `Already up to date` when nothing changes.

## See also

- [`grit history`](history.md) — other rewrite helpers (`reword`, `drop`, `squash`, …)
- [`grit pull --rebase`](pull.md) — fetch then rebase onto the remote tip
- [`grit push --force-with-lease`](push.md) — publish rewritten history safely
