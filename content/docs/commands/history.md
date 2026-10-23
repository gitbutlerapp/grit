---
title: grit history
summary: Stateless history rewrite helpers (reword, and more over time).
group: History
order: 6
---

## Synopsis

```text
grit history drop <commit>
grit history move <commit> (--before <anchor> | --after <anchor>)
grit history reword <commit> (-m <message> | --message-file <path|-> | --author <ident>)
grit history reword <commit> --author <ident> [-m <message>]
grit history drop|move|reword|fixup|squash|absorb|rebase … [--dry-run] [--update-refs=(branches|head)] [--empty=(drop|keep|abort)] [--keep-committer]
grit history fixup <commit> [-m|--message-file] [--hunks <id,…>] [--lines <path:start-end>] [-- <path>…]
grit history squash <range>… [-m|--message-file|--combine]
grit history absorb [--base <rev>]
grit history rebase [<upstream>] [--onto <rev>]
grit history split <commit> -m <message> [--rest-message <message>] -- <path>…
grit history undo [--dry-run]
```

## Description

`grit history` groups **stateless** rewrite operations: grit plans how commits and refs should change, writes new commit objects into the object database, then applies ref updates in one atomic batch. There is no sequencer state file and **no Git hooks** run during planning or apply. Rewrites preserve stable [change ids](../change-ids/) on surviving logical changes and store derived ids when the source commit had no header.

Shared behavior for every subcommand:

- **Conflicts abort** — if replaying a descendant would conflict, nothing is applied (planned objects may already exist in the ODB from planning).
- **`--dry-run`** — print which refs would move; refs, the index, and the worktree stay unchanged. New commit objects are still written during planning, matching Git’s `git history --dry-run` model. Commit signing is skipped during dry-run planning (no `gpg` / `ssh-keygen` invocation); a full run signs replacement commits when [`commit.gpgsign`](../commit/) is enabled, like [`grit commit`](../commit/).
- **`--update-refs`** — `branches` (default) updates every local branch whose tip descends from the edited commit; `head` updates only the commit `HEAD` points at.

### `drop`

Remove one commit from history and replay its descendants onto that commit’s first parent. The dropped commit’s tree changes are omitted; descendant commits are replayed with three-way merges like other history rewrites.

You cannot drop a **root** commit (no parent) or a **merge** commit itself. Descendant **merge** commits are replayed: mapped parents are updated and the merge tree is recomputed (first-parent transplant when side branches are unchanged, otherwise a fresh merge of the mapped parents).

### `move`

Reorder one commit on the **first-parent chain** of `HEAD` (no merge commits in the affected range). Grit replays the segment in the new order with the same cherry-pick semantics as other history helpers; branches pointing at commits in the segment move to the rewritten tips.

Provide exactly one of `--before` or `--after` naming another commit on that chain. Moving a commit to its current position is an error.

### `reword`

Replace one commit’s message and/or author and replay descendants onto the rewritten history. On the edited commit, tree and parents stay the same; only the message and/or author line change. Committer identity and time come from your configured committer and `GIT_COMMITTER_DATE`, like [`grit commit`](commit.md).

Provide at least one of `-m` / `--message`, `--message-file` (use `-` to read from stdin), or `--author`. The author value uses the same `Name <email>` / bare-email rules as [`grit commit --author`](commit.md); when you omit a date, grit keeps the edited commit’s author timestamp. Grit does not open an editor.

Descendant **merge** commits are replayed the same way as for [`drop`](#drop): parents are remapped and the merge tree is updated to match the rewritten first-parent chain.

### `fixup`

Fold **staged** changes into an earlier commit on the first-parent chain of `HEAD`, then replay descendants onto the rewritten history. Grit three-way merges staged edits into the target commit’s tree; descendant commits are replayed like other history rewrites. If the merge would make the target empty, grit aborts unless `--empty keep` or `--empty drop` is set (same shared `--empty` flag as other history subcommands).

Provide exactly one of `-m` / `--message` or `--message-file` when you want an explicit fixup message; otherwise grit uses the staged diff to build the amended commit.

### `absorb`

Assign **staged** hunks into the commits that last touched overlapping lines (between `--base` and `HEAD`), then replay descendants in **one** rewrite. Grit uses blame line mapping to find candidate commits and checks that each hunk commutes with commits after its target. Assigned hunks are folded into target commits via the same three-way merge as [`fixup`](#fixup); unassigned hunks stay staged.

`--base` is exclusive: commits at and before the base are not considered. When omitted, grit hides other local branch tips, applies an upstream fork-point when configured, and caps the stack at ten commits.

### `rebase`

Replay commits on the current branch onto `<upstream>` (or `--onto`), matching [`grit rebase`](rebase.md) including `--continue`, `--abort`, and `--skip`. Prefer the top-level command for everyday use; `grit history rebase` shares the same handler and JSON shape.

### `undo`

Restore branch tips (and the worktree when `HEAD`'s tree changed) from the last successful history rewrite that moved refs. Grit records each apply in `.git/grit/history-last` (ignored by `git fsck --strict`); `undo` inverts that plan when every recorded ref is still at its post-rewrite tip. A second `undo` redoes the rewrite. Recorded applies always write reflog entries for moved refs (even when `core.logAllRefUpdates=false`) so the pre-rewrite tips stay reachable until undo or reflog expiry.

There is nothing to undo when no record exists, when a listed ref moved, or when local changes would be overwritten by the checkout.

### `squash`

Fold every commit in a **connected** revision range into the **oldest** commit in that range, then replay descendants onto the new history. The squashed commit keeps the oldest commit's author and parents; its tree matches the range tip. By default the squashed commit keeps the oldest commit's message (including `amend!` handling inside the range). Use `-m` / `--message-file` for an explicit message, or `--combine` for an autosquash-style combined message from every commit in the range.

The range uses the same revision syntax as `git rev-list` (for example `HEAD~3..HEAD` or full OIDs `abc..def`). With `--update-refs head`, grit may fold at `HEAD` without replaying when the range ends at the current tip and the message mode allows it.

### `split`

Peel changes that match pathspecs out of one commit into a **new parent commit**, then leave the target commit’s tree unchanged and replay descendants. The new parent’s tree is the target’s first parent plus only the selected paths; the target commit keeps its author and tree (and its message unless `--rest-message` is set).

Provide `-m` / `--message-file` for the new parent commit and one or more paths after `--` (or `--pathspec-from-file`). Grit errors when the pathspec matches nothing or every change in the commit.

## Options

| Option | Description |
| --- | --- |
| `<commit>` | Commit to drop, move, or reword (revision spec). |
| `--before` | Place `<commit>` immediately before this revision (`move` only). |
| `--after` | Place `<commit>` immediately after this revision (`move` only). |
| `-m`, `--message` | New commit message (conflicts with `--message-file`). |
| `--message-file` | Read the new message from a file, or `-` for stdin. |
| `--author` | Replacement author ident (`Name <email>` or bare email). |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |
| `--empty` | `drop` (default), `keep`, or `abort` when a replay would become empty. |
| `--keep-committer` | Preserve committer ident and timestamp on replayed commits. |

### `split` options

| Option | Description |
| --- | --- |
| `<commit>` | Commit to split (revision spec). |
| `-m`, `--message` | Message for the new parent commit (conflicts with `--message-file`). |
| `--message-file` | Read the new parent message from a file, or `-` for stdin. |
| `--rest-message` | Replace the remaining commit’s message after the split. |
| `PATH`… | Pathspecs after `--` limiting which changes move into the new parent (required). |
| `--pathspec-from-file` | Read pathspecs from a file, or `-` for stdin (same as other commands). |
| `--pathspec-file-nul` | `--pathspec-from-file` entries are NUL-separated. |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |
| `--empty` | When a replayed commit would not change the tree: `drop` (default), `keep`, or `abort`. |
| `--keep-committer` | Keep each replayed commit’s original committer line instead of your configured committer. |

## Examples

### `squash` options

| Option | Description |
| --- | --- |
| `<range>…` | One or more revision specs defining the commits to fold (required). |
| `-m`, `--message` | Message for the squashed commit (conflicts with `--message-file` and `--combine`). |
| `--message-file` | Read the squashed commit message from a file, or `-` for stdin. |
| `--combine` | Combine messages from every commit in the range. |
| `--dry-run` | List ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |

### `absorb` options

| Option | Description |
| --- | --- |
| `--base` | Oldest commit boundary (exclusive) for absorb targets. |
| `--dry-run` | List assignments and ref updates without moving refs. |
| `--update-refs` | `branches` (default) or `head`. |

Move a commit before its parent on a linear branch:

```console
$ grit history move HEAD~1 --before HEAD~3
Rewrote 3 commit(s)
  commit a1b2c3d → d4e5f60
  refs/heads/main f6a7b89 → c0ffee0
```

Reword the parent of `HEAD` on a linear branch:

```console
$ grit history reword HEAD~1 -m " clearer subject line "
Rewrote 2 commit(s)
  commit a1b2c3d → d4e5f60
  refs/heads/main d4e5f60 → f6a7b89
```

Change only the author on an older commit (message and author date unchanged):

```console
$ grit history reword HEAD~2 --author "Ada Lovelace <ada@example.com>" --json
{"operation":"reword","rewritten":[…],"refs":[…],"head":{…},"dry_run":false}
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

Fold only selected **unstaged** hunks (same ids and `--lines` syntax as [`grit add`](add.md)) without staging the whole file first:

```console
$ grit diff --unstaged --json -- file.txt   # copy hunks[].id
$ grit history fixup HEAD~1 --hunks abcdef012345 -- file.txt
Fixed up 2 commit(s)
  commit a1b2c3d → d4e5f60
  refs/heads/main f6a7b89 → c0ffee0
```

Unknown or stale hunk ids exit **2** (same as `grit add --hunks`). Hunk ids and `--lines` ranges are resolved against the same index→worktree diff as [`grit diff --unstaged --json`](diff.md). Other unstaged hunks on the same paths stay in the worktree; pre-staged edits on those paths are preserved after apply, including when the selected hunk changes line count.

Absorb staged fixes on a linear branch:

```console
$ grit add file.txt
$ grit history absorb
Absorbed 1 hunk(s) into 2 commit(s)
  file.txt @@ -2,0 +3 @@ → a1b2c3d touch line 4
```

Preview hunk → commit assignment without changing refs or the index:

```console
$ grit add file.txt
$ grit history absorb --dry-run
Dry run — absorb would change 2 commit(s)
  file.txt @@ -2,0 +3 @@ → a1b2c3d touch line 4
  other.txt @@ -1,0 +2 @@ — no_candidate
  commit a1b2c3d → d4e5f60
  refs/heads/main f6a7b89 → c0ffee0
```

Split one file out of the parent of `HEAD`:

```console
$ grit history split HEAD~1 -m "add foo only" -- foo
Split 2 commit(s)
  commit a1b2c3d → d4e5f60
  refs/heads/main f6a7b89 → c0ffee0
```

## JSON output

Schemas: [drop v1](/schemas/history.drop/v1.json), [move v1](/schemas/history.move/v1.json), [reword v1](/schemas/history.reword/v1.json), [fixup v1](/schemas/history.fixup/v1.json), [squash v1](/schemas/history.squash/v1.json), [absorb v1](/schemas/history.absorb/v1.json), [undo v1](/schemas/history.undo/v1.json), [rebase v1](/schemas/history.rebase/v1.json), [split v1](/schemas/history.split/v1.json)

Pass `--json` on `grit history drop`, `grit history move`, `grit history reword`, `grit history fixup`, `grit history squash`, `grit history split`, `grit history absorb`, or `grit history undo`:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `operation` | string | `"drop"`, `"move"`, `"reword"`, `"fixup"`, `"squash"`, `"split"`, `"absorb"`, `"undo"`, or `"rebase"`. |
| `result` | string | `rebase` only: `completed`, `conflicts`, or `aborted`. |
| `rewritten` | array | `{old,new,change_id,subject}` full hex OIDs, stored change id (`null` when dropped or absent), and the first line of the replacement commit message (or the original when dropped). |
| `refs` | array | `{ref,old,new}` for each ref that would move or moved. `name` duplicates `ref` and is deprecated; it will be removed in a future release. |
| `head` | object or null | `{old,new}` when `HEAD`'s commit OID changes; `null` when unchanged. |
| `dry_run` | boolean | `true` when `--dry-run` was set. |
| `commit` | string | `rebase` when `result` is `conflicts`: source commit that stopped replay (full hex). |
| `conflicts` | array | `rebase` when `result` is `conflicts`: `{path,kind,summary?}`. |
| `next_steps` | array | `rebase` when paused: ordered follow-up `grit` commands. |
| `alternatives` | array | `rebase` when paused: `grit rebase --abort` and `grit rebase --skip`. |

For `rebase` with `--exec`, the object also includes:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `exec` | array | `{commit,command,exit_code}` per successful `--exec` invocation (full hex `commit`). |

For `absorb`, the object also includes:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `assignments` | array | `{path,hunk_range,commit,subject}` for hunks assigned to historical commits. |
| `unassigned` | array | `{path,hunk_range,reason}` for hunks left staged (`reason` is a stable code). |

For `rebase`, use the [`grit rebase`](rebase.md) JSON table (`result`, `commit`, `conflicts`, `next_steps`, `alternatives`, plus `rewritten` / `refs` / `head` when `result` is `completed`).

Example:

```json
{
  "operation": "reword",
  "rewritten": [
    {
      "old": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "new": "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d",
      "change_id": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
      "subject": "Reworded subject"
    }
  ],
  "refs": [
    {
      "ref": "refs/heads/main",
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

History rewrite commands other than `rebase` report planning conflicts as structured errors (`kind: "conflict"`). `grit history rebase` uses the same stop/resume JSON as [`grit rebase`](rebase.md) (`result: "conflicts"`, exit **1**).

## Markdown output

Schema: [v1](/schemas/history.reword/v1.json)

Pass [`--markdown`](../global-options/) for a heading such as `History squash` or `History reword` (matching `operation`), plus rewritten commits, ref updates, and optional `HEAD` movement.



```text
## History drop

- Dry run (refs unchanged)
- commit `e813550` → (dropped) — two
- commit `ac49499` → `5973cf5` — three
  - change id `nlwnxqt`
- `refs/heads/main` `ac49499` → `5973cf5`
- HEAD `ac49499` → `5973cf5`
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[Library guide: History rewrite](../library/history.md), [`grit log`](log.md), [`grit pick`](pick.md)