---
title: grit history
summary: Stateless history rewrite helpers (reword, and more over time).
group: History
order: 6
---

## Synopsis

```text
grit history reword <commit> -m <message>
grit history reword <commit> --message-file <path|->
grit history reword <commit> [--dry-run] [--update-refs=(branches|head)]
grit history squash <revision-range>... [--combine | -m <message> | --message-file <path|->]
grit history squash <revision-range>... [--dry-run] [--update-refs=(branches|head)]
```

## Description

`grit history` groups **stateless** rewrite operations: grit plans how commits and refs should change, writes new commit objects into the object database, then applies ref updates in one atomic batch. There is no sequencer state file and **no Git hooks** run during planning or apply.

Shared behavior for every subcommand:

- **Conflicts abort** — if replaying a descendant would conflict, nothing is applied (planned objects may already exist in the ODB from planning).
- **`--dry-run`** — print which refs would move; refs, the index, and the worktree stay unchanged. New commit objects are still written during planning, matching Git’s `git history --dry-run` model.
- **`--update-refs`** — `branches` (default) updates every local branch whose tip descends from the edited commit; `head` updates only the commit `HEAD` points at.

### `reword`

Replace one commit’s message and replay descendants onto the rewritten history. Author, tree, and parents stay the same on the edited commit; committer identity and time come from your configured committer and `GIT_COMMITTER_DATE`, like [`grit commit`](commit.md).

Provide exactly one of `-m` / `--message` or `--message-file` (use `-` to read from stdin). Grit does not open an editor.

If any descendant commit is a **merge**, planning refuses with an error (merge replay is not supported yet). Use a linear range or wait for later `grit history` subcommands that handle merges.

### `squash`

Fold a **revision range** into the oldest commit in that range, keep that commit’s author and parents (including merge parents when the oldest commit is a merge), take the range tip’s tree, and replay commits above the range unchanged. Range syntax matches `git rev-list` (`base..tip`, `^excl`, multiple tips).

Message mode (pick one; default keeps the oldest message, including `amend!` adjustments inside the range):

- **`--combine`** — autosquash-style combined message (comment lines from Git’s squash template are stripped; no editor).
- **`-m` / `--message-file`** — explicit message (same normalization as [`grit commit`](commit.md)).

`fixup!` / `squash!` / `amend!` commits whose target is outside the range are refused unless the range is only markers for one target. Squashing `squash!` without `--combine` or `-m` fails when a message would be discarded.

With **`--update-refs=branches`** (default), grit refuses when a local branch tip points at a commit inside the range but is not the range tip (use **`--update-refs=head`** to rewrite only the current branch).

## Options

| Option | Description |
| --- | --- |
| `<commit>` | Commit to reword (revision spec). |
| `<revision-range>` | One or more rev-list tokens (`A..B`, `^A`, tips). |
| `-m`, `--message` | New commit message (conflicts with `--message-file`; on `squash`, conflicts with `--combine`). |
| `--message-file` | Read the new message from a file, or `-` for stdin. |
| `--combine` | (`squash` only) Combine messages autosquash-style. |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |

## Examples

Reword the parent of `HEAD` on a linear branch:

```console
$ grit history reword HEAD~1 -m " clearer subject line "
Rewrote 2 commit(s)
  commit a1b2c3d → d4e5f60
  refs/heads/main d4e5f60 → f6a7b89
```

Preview ref movements without updating branches:

```console
$ grit history reword HEAD~1 -m "new message" --dry-run
Dry run — refs would change:
  commit a1b2c3d → d4e5f60
  refs/heads/main f6a7b89 → c0ffee0
```

## JSON output

Schemas: [reword v1](/schemas/history.reword/v1.json), [squash v1](/schemas/history.squash/v1.json)

Pass `--json` on `grit history reword` or `grit history squash`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `operation` | string | `"reword"` or `"squash"`. |
| `rewritten` | array | `{old,new}` full hex OIDs for each rewritten commit. |
| `refs` | array | `{name,old,new}` for each ref that would move or moved. |
| `head` | object or null | `{old,new}` when `HEAD`'s commit OID changes; `null` when unchanged. |
| `dry_run` | boolean | `true` when `--dry-run` was set. |

Example:

```json
{
  "operation": "reword",
  "rewritten": [
    {
      "old": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "new": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"
    }
  ],
  "refs": [
    {
      "name": "refs/heads/side",
      "old": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
      "new": "9f8e7d6c5b4a392817061524334352637281910"
    }
  ],
  "head": null,
  "dry_run": false
}
```

On replay conflicts, stdout is a single JSON object (exit code 1) with `error`, `kind` (`conflict`), and `conflicts` (sorted paths), same shape as [`grit pick`](pick.md).

## Markdown output

Schema: [v1](/schemas/history.reword/v1.json)

Pass [`--markdown`](../global-options/) for a `History reword` heading, rewritten commits, ref updates, and optional `HEAD` movement.

## See also

[Library guide: History rewrite](../library/history.md), [`grit log`](log.md), [`grit pick`](pick.md)
