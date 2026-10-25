---
title: grit merge
summary: Merge another branch into the current one.
group: Branches and tags
order: 3
---

## Synopsis

```text
grit merge <branch>
grit merge --check <branch>
grit merge --dry-run <branch>
grit merge --continue [-m <message>]
grit merge --abort
grit merge [--ff-only | --no-ff] [--squash | --no-commit] [-X ours|theirs] [--conflict diff3|zdiff3|merge] [--allow-unrelated-histories] [-m <message> | -F / --message-file <path|->] <branch>
```

## Description

Brings the commits from `<branch>` into the current branch:

- If the current branch already has everything, nothing changes.
- If the current branch has no commits of its own since they diverged, it's moved forward to `<branch>` (a fast-forward). No new commit is made.
- Otherwise both sides are merged. When there are no overlapping edits, a merge commit is recorded with the message `Merge <branch>`. That commit gets a new `change-id` header like [`grit commit`](../commit/). When `commit.gpgsign` is enabled, that merge commit is signed like [`grit commit`](../commit/).
- When the same lines were changed on both sides, `grit merge` stops with conflicts: it updates the index and working tree, writes conflict markers around each conflicting hunk, records merge-in-progress state (`MERGE_HEAD`, `ORIG_HEAD`, `MERGE_MSG`), and leaves the branch ref unchanged. Fix conflicts, then run `grit merge --continue` to create the two-parent merge commit (message from `MERGE_MSG` with comment lines stripped, or `-m` to override). When `commit.autoStage` is true (the default), `--continue` stages resolved paths from the work tree the same way [`grit commit`](../commit/) does, so you do not need `grit add` after editing away conflict markers. With `commit.autoStage=false`, stage resolved paths with `grit add` before `--continue`. [`grit commit`](../commit/) can also finish a resolved merge. Run `grit merge --abort` to discard the in-progress merge and restore the pre-merge branch, index, and working tree.
- After a merge with `--no-commit` (when supported), run `grit merge --continue` to record the merge commit without re-running the three-way merge.

After a fast-forward or a merge commit, grit runs the `post-merge` hook with `0`; after `--squash` it passes `1`, as Git does. The hook does not run when the branch is already up to date, when the merge stops on conflicts, with `--no-commit`, or for `--check` and `--dry-run`. `grit merge --continue` runs `post-commit` like [`grit commit`](../commit/), not `post-merge` (Git behaves the same). The hook's exit status never changes the result. See [`grit hooks`](../hooks/).

`<branch>` can be a local branch or a remote-tracking branch such as `origin/main`. Local branches are looked up first.

Local edits are allowed when they only touch paths the merge does not change, or when a changed path already matches the post-merge tree in the index (same scoped rule as [`grit switch`](../switch/)). Grit refuses when your changes would be overwritten by files the merge updates, when an untracked file would be replaced by a path the merge would check out, on a branch with no commits, or with a detached HEAD; the branch and file are left unchanged.

### Merge modes

- **`--ff-only`** — Refuse when a fast-forward is not possible (histories diverged). Nothing changes. Exit code **128** (same as Git's `merge --ff-only`).
- **`--no-ff`** — Always record a merge commit with two parents when a real merge is needed, even if a fast-forward was possible.
- **`--squash`** — Apply the merged tree to the index and worktree, write a Git-compatible `SQUASH_MSG`, and do not update the branch ref or write `MERGE_HEAD`. Finish with [`grit commit`](../commit/) (which uses `SQUASH_MSG` as the default message when you omit `-m`).
- **`--no-commit`** — Perform the merge but stop before committing: writes `MERGE_HEAD`, `ORIG_HEAD`, and `MERGE_MSG` (and `MERGE_MODE` when `--no-ff` applies). Finish with [`grit commit`](../commit/) or `grit merge --continue` (when implemented).

`-m` / `-F` / `--message-file` override the default merge commit message and the text stored in `MERGE_MSG` on conflicts or `--no-commit`.

**`-X ours|theirs`** — When both sides edited the same region, prefer the current branch (`ours`) or the branch being merged (`theirs`) instead of leaving conflict markers. Matches Git's merge strategy options (union is not exposed).

**`--conflict diff3|zdiff3|merge`** — Conflict marker style in the worktree. When omitted, grit reads `merge.conflictStyle` (default `merge`).

**`--allow-unrelated-histories`** — Merge branches that share no commits, using the empty tree as the merge base (Git-compatible). Without this flag, grit refuses unrelated histories.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the current one. |
| `--check` | Predict whether the merge would succeed without changing refs, the index, or the working tree. Exit **0** when it would fast-forward, already be up to date, or merge cleanly; exit **1** when it would conflict. Other failures use the usual error exit. |
| `--dry-run` | Report the planned merge outcome (up to date, fast-forward, merge commit with parents, or conflicts) without changing refs, the index, or the working tree. Exit **0** unless the merge would be refused (for example `--ff-only` when a fast-forward is not possible, exit **128**). Conflicts are reported in the JSON but do not fail the command. Mutually exclusive with `--check`. |
| `--continue` | Finish an in-progress merge after conflicts are resolved (or after `--no-commit`). Auto-stages resolved paths when `commit.autoStage` is true (default). Conflicts with `<branch>`, `--check`, and `--abort`. |
| `-m`, `--message` | Commit message for `--continue` only (overrides `MERGE_MSG`). |
| `--abort` | Cancel an in-progress merge and restore the state recorded in `ORIG_HEAD`. |
| `--ff-only` | Refuse when a fast-forward is not possible (exit code 128). Mutually exclusive with `--no-ff`. |
| `--no-ff` | Create a merge commit even when a fast-forward is possible. Mutually exclusive with `--ff-only`. |
| `--squash` | Squash merged commits into the index; write `SQUASH_MSG` (no `MERGE_HEAD`). Mutually exclusive with `--no-commit`. |
| `--no-commit` | Apply the merge but do not commit yet (Git-compatible merge state). Mutually exclusive with `--squash`. |
| `-m`, `--message` | Merge commit / `MERGE_MSG` message (conflicts with `--message-file`). |
| `-F`, `--message-file` | Read the message from a path, or `-` for stdin (conflicts with `-m`). |
| `-X ours\|theirs` | Prefer one side on overlapping edits (same as Git `-X`). |
| `--conflict` | Marker style: `merge`, `diff3`, or `zdiff3`. |
| `--allow-unrelated-histories` | Merge with no common ancestor (empty-tree base). |

## Examples

```console
$ grit merge feature
Fast-forwarded feature → cf18394

$ grit merge origin/main
Merged origin/main into the current branch (acd1d4b)

$ grit merge --no-ff topic
Merged topic into the current branch (1a2b3c4)

$ grit merge --squash topic
Squash merge applied from topic — run `grit commit` to record the squashed commit

$ grit merge --no-commit topic
Merge from topic applied — run `grit commit` to finish, or `grit merge --abort` to cancel
```

When both branches change the same lines:

```console
$ grit merge topic
CONFLICT (content): README.md

Automatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`.
```

When one branch deleted a file and the other modified it, grit leaves the modified side's content in the work tree (like Git) so you can run `grit add <path>` to keep it or `grit rm <path>` to accept the deletion:

```console
$ grit merge feat
CONFLICT (modify_delete): f deleted on the current branch and modified in feat; feat's version is in the work tree

Automatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`.
```

After resolving markers (staging is optional when `commit.autoStage` is true):

```console
$ grit merge --continue
Merged topic into the current branch (acd1d4b)
```

To cancel:

```console
$ grit merge --abort
Merge aborted.
```

When an untracked file would be overwritten:

```console
$ grit merge feature
error: untracked file 'notes.txt' would be overwritten — move or remove it first
```

When `--ff-only` cannot fast-forward:

```console
$ grit merge --ff-only topic
Cannot fast-forward topic — histories have diverged (use merge without --ff-only)
```

(Exit code 128.)

## JSON output

Schema: [v2](/schemas/merge/v2.json) · [v1](/schemas/merge/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `clean` (`--check` only), `merged`, `squashed`, `stopped`, `conflicts`, `refused_not_ff`, `aborted`, or `set_upstream`. |
| `check` | boolean | `true` when this outcome is from `--check` (nothing was merged). Omitted for real merges. |
| `dry_run` | boolean | `true` when this outcome is from `--dry-run` (nothing was merged). Omitted for real merges. |
| `branch` | string | The branch that was merged in (or set as upstream). |
| `oid` | string or null | Resulting commit after a real merge or fast-forward; for `--check` with `result: clean`, the predicted merge tree OID. Omitted for `--dry-run` when a new merge commit would be created. |
| `parents` | array or null | For `--dry-run` with `result: merged`, the two parent commit ids of the merge commit that would be recorded. |
| `change_id` | string or null | Stored `change-id` on `oid` when present (merge commit or fast-forward tip). |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |
| `conflicts` | array or null | When `result` is `conflicts`, each item has `path`, `kind` (`content`, `add_add`, or `modify_delete`), and optional `summary` text. |
| `next_steps` | array or null | Ordered follow-up commands when `result` is `conflicts` (for example `grit diff`, `grit add .`), or suggested commands when `result` is `squashed` or `stopped`. |
| `alternatives` | array or null | Mutually exclusive options such as `grit merge --abort` (not part of the ordered sequence). |

Example:

```json
{
  "result": "up_to_date",
  "branch": "main"
}
```

Dry-run prediction with `--check` (exit code 0):

```json
{
  "result": "clean",
  "check": true,
  "branch": "topic",
  "oid": "4b825dc642cb6eb9a06054e089d91b5929f0d0a"
}
```

Squash merge:

```json
{
  "result": "squashed",
  "branch": "topic",
  "next_steps": [
    "grit commit",
    "Review SQUASH_MSG or pass -m / --message-file"
  ]
}
```

When the merge stops on conflicts (exit code 1), stdout is still a single JSON object:

```json
{
  "result": "conflicts",
  "branch": "topic",
  "conflicts": [
    {
      "path": "src/lib.rs",
      "kind": "content"
    }
  ],
  "next_steps": ["grit diff", "grit add ."],
  "alternatives": ["grit merge --abort"]
}
```

## Markdown output

Schema: [v2](/schemas/merge/v2.json) · [v1](/schemas/merge/v1.json)
Pass [`--markdown`](../global-options/) for a `Merge` heading and a result summary. Fast-forward:

```text
- Fast-forwarded **`main`** → `9244cbd`
```

When the merge stops on conflicts (exit code 1), stdout still carries structured Markdown with conflict paths and next steps:

```text
- Merge **`side`** stopped with conflicts

- `f.txt`

→ fix conflicts, then `grit merge --continue`, or `grit merge --abort`
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Merge completed, `--check` predicts success, or `--dry-run` preview. |
| `1` | Conflicts (`result: conflicts`) or other failure ([`failure`](../exit-codes/)). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| `128` | `--ff-only` when not fast-forward ([`refused_not_ff`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit pull](../pull/), [grit pick](../pick/), [grit branch](../branch/)