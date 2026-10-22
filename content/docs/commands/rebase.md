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
grit rebase|history rebase … [--dry-run] [--update-refs=(branches|head)] [--empty=(drop|keep|abort)] [--keep-committer]
```

## Description

`grit rebase` replays commits reachable from `HEAD` but not from `<upstream>` onto `<upstream>` (or onto `--onto` when set). It uses the same stateless history engine as [`grit history`](history.md): grit plans the rewrite, writes new commit objects, then updates refs in one batch. There is no sequencer directory and **no Git hooks** during planning or apply.

When `<upstream>` is omitted, grit uses `@{upstream}` of the current branch. If no upstream is configured, the command fails with a hint to run [`grit branch -u`](branch.md).

Conflicts during planning abort atomically: exit **1**, structured JSON with `kind: "conflict"`, conflicting commit and paths, and **no** change to refs, the index, or the worktree. Stop/continue rebase is planned separately.

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

## Examples

```console
$ grit branch -u origin/main
$ grit rebase --json
{
  "operation": "rebase",
  "rewritten": [ … ],
  "refs": [ … ],
  "head": { "old": "…", "new": "…" },
  "dry_run": false
}
```

```console
$ grit rebase --onto release/v2 origin/main --dry-run
Dry run — refs would change:
  commit abc1234 → def5678 (feature work)
  refs/heads/feature abc1234 → def5678
```

## JSON output

Schema: [v1](/schemas/rebase/v1.json)

| Field | Type | Meaning |
| --- | --- | --- |
| `operation` | string | Always `rebase`. |
| `rewritten` | array | Old/new OIDs, optional `change_id` and `subject` per commit. |
| `refs` | array | `ref`, `old`, `new` (and deprecated `name`) for each moved ref. |
| `head` | object or null | Previous and new `HEAD` commit when it changes. |
| `dry_run` | boolean | Whether refs were left unchanged. |

On conflict, stdout is a structured error (`kind: "conflict"`, `commit`, `conflicts`) and exit code is **1**.

## Markdown output

Headings under `## Rebase` (or `## History rebase` for the alias). Lists rewritten commits and ref updates; `Already up to date` when nothing changes.

## See also

- [`grit history`](history.md) — other rewrite helpers (`reword`, `drop`, `squash`, …)
- [`grit pull --rebase`](pull.md) — fetch then rebase onto the remote tip
- [`grit push --force-with-lease`](push.md) — publish rewritten history safely
