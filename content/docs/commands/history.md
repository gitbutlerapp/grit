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

grit history split <commit> -m <message> [--] <pathspec>...
grit history split <commit> --message-file <path|-> [--] <pathspec>...
grit history split <commit> ... [--message-rest <message>|--message-rest-file <path|->]
grit history split <commit> ... [--dry-run] [--update-refs=(branches|head)]
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

### `split`

Move changes that match at least one **pathspec** into a new parent commit, then replay descendants. The edited commit keeps its tree (and author); only its first parent becomes the new commit. The split commit uses the original author and your `-m` / `--message-file` message. Use `--message-rest` when the remaining commit should get a new message.

At least one pathspec is required (pass `--` before pathspecs when they might look like flags). Interactive hunk selection is **not** supported — pathspecs only.

Splitting **merge commits** is refused. Selecting no changes (`SplitSelectsNothing`) or every change in the commit (`SplitSelectsEverything`) is an error.

## Options

Shared (`reword` and `split`):

| Option | Description |
| --- | --- |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |

### `reword`

| Option | Description |
| --- | --- |
| `<commit>` | Commit to reword (revision spec). |
| `-m`, `--message` | New commit message (conflicts with `--message-file`). |
| `--message-file` | Read the new message from a file, or `-` for stdin. |

### `split`

| Option | Description |
| --- | --- |
| `<commit>` | Commit to split (revision spec). |
| `-m`, `--message` | Message for the new parent commit (conflicts with `--message-file`). |
| `--message-file` | Read the split message from a file, or `-` for stdin. |
| `--message-rest` | New message for the remaining commit (conflicts with `--message-rest-file`). |
| `--message-rest-file` | Read the remaining message from a file, or `-` for stdin. |
| `<pathspec>…` | One or more pathspecs (required). |

## Examples (reword)

Reword the parent of `HEAD` on a linear branch:

```console
$ grit history reword HEAD~1 -m " clearer subject line "
History reword: 2 commit(s) rewritten
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

## Examples (split)

Split one file out of the tip commit:

```console
$ grit history split HEAD -m "add foo only" -- foo
History split: 1 commit(s) rewritten
  commit a1b2c3d → d4e5f60
  refs/heads/main a1b2c3d → d4e5f60
```

## JSON output (reword)

Schema: [v1](/schemas/history.reword/v1.json)

Pass `--json` on `grit history reword`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `operation` | string | Always `"reword"`. |
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
      "name": "refs/heads/main",
      "old": "9f8e7d6c5b4a392817061524334352637281910",
      "new": "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567"
    }
  ],
  "head": {
    "old": "9f8e7d6c5b4a392817061524334352637281910",
    "new": "0a1b2c3d4e5f60718293a4b5c6d7e8f901234567"
  },
  "dry_run": false
}
```

## JSON output (split)

Schema: [v1](/schemas/history.split/v1.json)

Same fields as reword; `operation` is `"split"`.

## Markdown output

Schemas: [reword v1](/schemas/history.reword/v1.json), [split v1](/schemas/history.split/v1.json)

Pass [`--markdown`](../global-options/) for a `History <operation>` heading, rewritten commits, ref updates, and optional `HEAD` movement.

On replay conflicts, stdout is a single JSON object (exit code 1) with `error`, `kind` (`conflict`), and `conflicts` (sorted paths), same shape as [`grit pick`](pick.md).

## See also

[Library guide: History rewrite](../library/history.md), [`grit log`](log.md), [`grit pick`](pick.md)
