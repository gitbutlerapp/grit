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
grit history fixup <commit> [--empty=(drop|keep|abort)] [-m|--message-file]
grit history fixup <commit> [--dry-run] [--update-refs=(branches|head)]
grit history absorb [--base <rev>] [--dry-run] [--update-refs=(branches|head)]
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

### `fixup`

Apply **staged** index changes onto an earlier commit and replay descendants. Grit three-way merges `HEAD`'s tree (base), the target commit's tree (ours), and the index tree (theirs), matching upstream `git history fixup`. The target keeps its author and message unless you pass `-m` / `--message-file`.

Requires a working tree with at least one staged change relative to `HEAD`. Bare repositories are rejected. On merge conflicts, nothing is applied.

After a successful apply, staged changes on the paths you fixed are consumed: the index matches the new `HEAD` for those paths, and unrelated unstaged edits stay in the worktree.

### `absorb`

Assign **staged** hunks into the commits that last touched overlapping lines (between `--base` and `HEAD`), then replay descendants in **one** rewrite. Grit uses blame line mapping to find candidate commits and checks that each hunk commutes with commits after its target. Assigned hunks are folded into target commits via the same three-way merge as [`fixup`](#fixup); unassigned hunks stay staged.

`--base` is exclusive: commits at and before the base are not considered. When omitted, grit hides other local branch tips, applies an upstream fork-point when configured, and caps the stack at ten commits.

## Options

| Option | Description |
| --- | --- |
| `<commit>` | Commit to reword (revision spec). |
| `-m`, `--message` | New commit message (conflicts with `--message-file`). |
| `--message-file` | Read the new message from a file, or `-` for stdin. |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |

### `fixup` options

| Option | Description |
| --- | --- |
| `<commit>` | Commit to fix up (revision spec). |
| `--empty` | `drop` (default), `keep`, or `abort` when the target would become empty. |
| `-m`, `--message` | Replace the target commit message (optional). |
| `--message-file` | Read replacement message from a file, or `-` for stdin. |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |

### `absorb` options

| Option | Description |
| --- | --- |
| `--base` | Oldest commit boundary (exclusive) for absorb targets. |
| `--dry-run` | List assignments and ref updates without moving refs. |
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

Fix up the parent of `HEAD` using staged edits:

```console
$ grit add file.txt
$ grit history fixup HEAD~1
Fixed up 2 commit(s)
  commit a1b2c3d → d4e5f60
  refs/heads/main f6a7b89 → c0ffee0
```

Absorb staged fixes on a linear branch:

```console
$ grit add file.txt
$ grit history absorb
Absorbed 1 hunk(s) into 2 commit(s)
  file.txt → commit a1b2c3d
```

## JSON output

Schema: [v1](/schemas/history.reword/v1.json) (`reword`), [v1](/schemas/history.fixup/v1.json) (`fixup`), [v1](/schemas/history.absorb/v1.json) (`absorb`)

Pass `--json` on `grit history reword`, `grit history fixup`, or `grit history absorb`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `operation` | string | `"reword"` or `"fixup"`. |
| `rewritten` | array | `{old,new}` full hex OIDs for each rewritten commit. |
| `refs` | array | `{name,old,new}` for each ref that would move or moved. |
| `head` | object or null | `{old,new}` when `HEAD`'s commit OID changes; `null` when unchanged. |
| `dry_run` | boolean | `true` when `--dry-run` was set. |

For `absorb`, the object also includes:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `assignments` | array | `{path,hunk_range,commit}` for hunks assigned to historical commits. |
| `unassigned` | array | `{path,hunk_range,reason}` for hunks left staged (`reason` is a stable code). |

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

Schema: [v1](/schemas/history.reword/v1.json) (`reword`), [v1](/schemas/history.fixup/v1.json) (`fixup`)

Pass [`--markdown`](../global-options/) for a `History reword` or `History fixup` heading, rewritten commits, ref updates, and optional `HEAD` movement.

## See also

[Library guide: History rewrite](../library/history.md), [`grit log`](log.md), [`grit pick`](pick.md)
