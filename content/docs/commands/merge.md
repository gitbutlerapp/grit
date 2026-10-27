---
title: grit merge
summary: Merge another branch, tag or commit into the current one.
group: Branches and tags
order: 3
---

## Synopsis

```text
grit merge <rev>
grit merge --check <rev>
grit merge --dry-run <rev>
grit merge --continue [-m <message>]
grit merge --abort
grit merge [--ff-only | --no-ff] [--squash | --no-commit] [-X ours|theirs] [--conflict diff3|zdiff3|merge] [--allow-unrelated-histories] [--no-verify] [-m <message> | -F / --message-file <path|->] <rev>
```

## Description

Brings the commits from `<rev>` into the current branch:

- If the current branch already has everything, nothing changes.
- If the current branch has no commits of its own since they diverged, it's moved forward to `<rev>` (a fast-forward). No new commit is made.
- Otherwise both sides are merged. When there are no overlapping edits, a merge commit is recorded with the message `Merge <rev>`, using `<rev>` as you typed it. That commit gets a new `change-id` header like [`grit commit`](../commit/). When `commit.gpgsign` is enabled, that merge commit is signed like [`grit commit`](../commit/).
- When the same lines were changed on both sides, `grit merge` stops with conflicts: it updates the index and working tree, writes conflict markers around each conflicting hunk, records merge-in-progress state (`MERGE_HEAD`, `ORIG_HEAD`, `MERGE_MSG`), and leaves the branch ref unchanged. Fix conflicts, then run `grit merge --continue` to create the two-parent merge commit (message from `MERGE_MSG` with comment lines stripped, or `-m` to override). When `commit.autoStage` is true (the default), `--continue` stages resolved paths from the work tree the same way [`grit commit`](../commit/) does, so you do not need `grit add` after editing away conflict markers. With `commit.autoStage=false`, stage resolved paths with `grit add` before `--continue`. [`grit commit`](../commit/) can also finish a resolved merge. Run `grit merge --abort` to discard the in-progress merge and restore the pre-merge branch, index, and working tree.
- After a merge with `--no-commit` (when supported), run `grit merge --continue` to record the merge commit without re-running the three-way merge.

Before a clean merge records its commit, grit runs the same hooks as Git, in Git's order: `pre-merge-commit` (no arguments), then `prepare-commit-msg .git/MERGE_MSG merge` and `commit-msg .git/MERGE_MSG`, which see `MERGE_HEAD` and may rewrite the message (the commit records what they leave). Files a `pre-merge-commit` hook stages go into the merge commit. When one of them exits non-zero, the merge stops with exit **1** and a `hook_failed` error (as for [`grit commit`](../commit/)): the merged files stay in the work tree and index, `MERGE_HEAD`, `ORIG_HEAD` and `MERGE_MSG` are recorded, and the branch does not move. Finish with `grit merge --continue` (which runs `pre-commit` like `grit commit`, as Git does) or cancel with `grit merge --abort`; edits a [`grit pull --autostash`](../pull/) stashed come back either way. `--no-verify` skips `pre-merge-commit` and `commit-msg`; `prepare-commit-msg` and `post-merge` still run. None of these run for a fast-forward, `--squash`, `--no-commit`, conflicts, `--check` or `--dry-run`.

```console
$ grit merge topic -m 'WIP merge'
pre-merge-commit: blocked
error: hook pre-merge-commit failed with exit status 1
hint: the merge is still in progress: run `grit merge --continue` to commit it, or `grit merge --abort` to cancel
$ grit merge --abort
Merge aborted.
```

With `--json` the error carries the hook's output and the same `hint`:

```json
{
  "error": "hook pre-merge-commit failed with exit status 1",
  "hint": "the merge is still in progress: run `grit merge --continue` to commit it, or `grit merge --abort` to cancel",
  "hook": {
    "name": "pre-merge-commit",
    "status": 1,
    "stderr": "pre-merge-commit: blocked\n",
    "stdout": ""
  },
  "kind": "hook_failed"
}
```

After a fast-forward or a merge commit, grit runs the `post-merge` hook with `0`; after `--squash` it passes `1`, as Git does. The hook does not run when the branch is already up to date, when the merge stops on conflicts, with `--no-commit`, or for `--check` and `--dry-run`. `grit merge --continue` runs `post-commit` like [`grit commit`](../commit/), not `post-merge` (Git behaves the same). The hook's exit status never changes the result. See [`grit hooks`](../hooks/).

Every merge records the commit the branch was on in `ORIG_HEAD`, as `git merge` does: an up-to-date merge, a fast-forward, a merge commit, `--squash`, `--no-commit`, a merge that stops on conflicts, and a `--ff-only` merge refused because it cannot fast-forward. It replaces any earlier value and stays after the merge finishes, `--continue` or `--abort`, so `grit reset --hard ORIG_HEAD` undoes a merge you regret. `--check` and `--dry-run` leave it alone.

`<rev>` can be any branch, tag or commit: a local branch, a remote-tracking branch such as `origin/main`, a tag such as `v1.2` (annotated tags merge the commit they point at), a full or abbreviated commit id, or a revision expression such as `topic~2`, `origin/main^` or `ORIG_HEAD`. A name is looked up as a local branch first, then a remote-tracking branch, then a tag, then as a revision. Ranges (`a..b`, `a...b`) and trees or blobs are refused.

Literal braces are allowed in ref names: `grit merge 'foo{bar'` merges that branch, while `grit merge 'foo{bar...main'` refuses the range without moving HEAD. Dots within selectors, such as `main^{/release..candidate}`, remain part of a single revision.

On a criss-cross history the two branches have several merge bases (`grit merge-base --all` lists them). grit then merges those bases into one virtual base first and merges the two branches against it, like Git, so `grit merge` and `grit merge --check` give the same answer whichever base comes first. When the two sides resolved the same old conflict in different ways, the merge stops with a conflict instead of silently keeping one resolution. In that case the base version (stage 1 in the index, the `|||||||` section with `--conflict diff3`) holds the bases' own conflict, with markers labelled `Temporary merge branch 1` and `Temporary merge branch 2`. `-X ours|theirs` applies only to the final merge, not to the merge of the bases.

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

On an interactive terminal (human output, not `--json` or `--markdown`), a live **Checkout** line on stderr counts the files the merge writes, and `--continue` shows a **Staging** line while it auto-stages; nothing is drawn when stderr is not a TTY.

### Directory renames

When one branch moved every file out of a directory (renaming `a/` to `b/`) and the other branch added a file inside the old directory, the new file most likely belongs in `b/`. Grit detects this the way Git does: the directory must be gone on the renaming side, and its destination is the directory that received the most of its files. The `merge.directoryRenames` config decides what happens:

- **`conflict`** (default, as in Git) — The file is moved to `b/` and left unmerged so you can confirm the location. It's in the work tree at the new path; stage it there (or move it back) and run `grit merge --continue`.
- **`true`** — The file is moved to `b/` and the merge is clean.
- **`false`** — The file stays in `a/`.

Any other value is an error. With `conflict` or `true`, two cases still stop the merge without moving anything, and leave nothing unmerged: a directory whose files went to several directories with no clear winner (`directory_rename_split`), and a new file whose destination already exists on the branch that added it, or would be shared with another new file (`directory_rename_blocked`). When only the base or the renaming branch has a file at the destination, the moved file is merged with it there, as Git does; that can stop with an ordinary conflict at the new path, such as `deleted_by_us` when the renaming branch deleted that file. Review the placement, then run `grit merge --continue`. [`grit pick`](../pick/), [`grit revert`](../revert/), [`grit rebase`](../rebase/) and [`grit pull`](../pull/) follow the same setting.

```console
$ grit merge theirs
CONFLICT (directory_rename_moved): a/new was added to a directory the other side renamed, so it now sits at b/new; stage it there or move it back

Automatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`.

$ grit add b/new
$ grit merge --continue
Merged theirs into the current branch (06e1c12)
```

## Options

| Option | Description |
| --- | --- |
| `<rev>` | The branch, tag or commit to merge into the current one. |
| `--check` | Predict whether the merge would succeed without changing refs, the index, or the working tree. Exit **0** when it would fast-forward, already be up to date, or merge cleanly; exit **1** when it would conflict. Other failures use the usual error exit. |
| `--dry-run` | Report the planned merge outcome (up to date, fast-forward, merge commit with parents, or conflicts) without changing refs, the index, or the working tree. Exit **0** unless the merge would be refused (for example `--ff-only` when a fast-forward is not possible, exit **128**). Conflicts are reported in the JSON but do not fail the command. Mutually exclusive with `--check`. |
| `--continue` | Finish an in-progress merge after conflicts are resolved (or after `--no-commit`). Auto-stages resolved paths when `commit.autoStage` is true (default). Conflicts with `<rev>`, `--check`, and `--abort`. |
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
| `--no-verify` | Skip the `pre-merge-commit` and `commit-msg` hooks (`prepare-commit-msg` and `post-merge` still run). Not with `--continue`, `--abort`, `--check` or `--dry-run`. |

## Examples

```console
$ grit switch old
$ grit merge main
Fast-forwarded old → cf18394 (main)

$ grit merge origin/main
Merged origin/main into the current branch (acd1d4b)

$ grit merge --no-ff topic
Merged topic into the current branch (1a2b3c4)

$ grit merge v1.2
Merged v1.2 into the current branch (15d4bf9)

$ grit merge a79778433f24ac0955d38d912c13644b0920de68
Merged a79778433f24ac0955d38d912c13644b0920de68 into the current branch (e3ca27d)

$ grit merge --squash topic
Squash merge applied from topic — run `grit commit` to record the squashed commit

$ grit merge --no-commit topic
Merge from topic applied — run `grit commit` to finish, or `grit merge --abort` to cancel
```

When both branches change the same lines:

```console
$ grit merge topic
CONFLICT (both_modified): README.md

Automatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`.
```

When one branch deleted a file and the other modified it, grit leaves the modified side's content in the work tree (like Git) so you can run `grit add <path>` to keep it or `grit rm <path>` to accept the deletion:

```console
$ grit merge feat
CONFLICT (deleted_by_us): f deleted on the current branch and modified in feat; feat's version is in the work tree

Automatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`.
```

When both branches renamed the same file to different names, grit cannot pick a name, so it stops (like Git) and keeps both names in the work tree. Each one holds the merged content. The old name stays unmerged in the index next to the two new names. To keep one name, remove the old name and the other new name with `grit rm`, then run `grit merge --continue`. `grit merge --check` exits **1** for this case.

```console
$ grit merge topic
CONFLICT (rename_rename): a renamed to b on the current branch and to c in topic; both names are in the work tree
CONFLICT (rename_rename): b
CONFLICT (rename_rename): c

Automatic merge failed; fix conflicts and run `grit merge --continue`, or run `grit merge --abort`.

$ grit rm a c
Removed 2 paths.

$ grit merge --continue
Merged topic into the current branch (b3e1fe8)
```

When the branches put two different files at the same path (each renamed a different file to it, or one renamed a file onto a path the other added), grit reports a `both_added` conflict at that path, like Git. Both versions stay in the index and the work-tree file shows them between conflict markers; a renamed file already carries the other branch's edits to it. The old names are gone, so nothing is lost until you resolve the path.

```console
$ grit merge topic
CONFLICT (both_added): c

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

When nothing has that name:

```console
$ grit merge nonexist
error: no branch, tag or commit named 'nonexist'
```

When `--continue` runs while conflict markers are still in a file:

```console
$ grit merge --continue
error: you still have unmerged paths: notes.txt
hint: resolve the conflict markers in notes.txt, then run `grit merge --continue` (or `grit merge --abort` to start over)
```

When local edits on a path the merge would update would be overwritten:

```console
$ grit merge feature
error: local changes would be overwritten by merge
  a.txt
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
| `result` | string | `up_to_date`, `fast_forward`, `clean` (`--check` only), `merged`, `squashed`, `stopped`, `conflicts`, `refused_not_ff`, or `aborted`. |
| `check` | boolean | `true` when this outcome is from `--check` (nothing was merged). Omitted for real merges. |
| `dry_run` | boolean | `true` when this outcome is from `--dry-run` (nothing was merged). Omitted for real merges. |
| `branch` | string | The branch, tag or commit that was merged in, exactly as given on the command line. |
| `updated_branch` | string or null | After a fast-forward, the current branch whose tip moved. Omitted when it matches `branch`. |
| `oid` | string or null | Resulting commit after a real merge or fast-forward; for `--check` with `result: clean`, the predicted merge tree OID (the id `git merge-tree --write-tree` prints; `--check` does not store that tree, so it is not in the object database). Omitted for `--dry-run` when a new merge commit would be created. |
| `parents` | array or null | For `--dry-run` with `result: merged`, the two parent commit ids of the merge commit that would be recorded. |
| `change_id` | string or null | Stored `change-id` on `oid` when present (merge commit or fast-forward tip). |
| `conflicts` | array or null | When `result` is `conflicts`, each item has `path`, a [conflict `kind`](../status/#conflict-kinds) (the same names `grit status` uses, plus `rename_rename` and the `directory_rename_*` kinds), and optional `summary` text for `deleted_by_us`, `deleted_by_them`, `rename_rename` and directory rename conflicts. A `rename_rename` conflict lists all three paths (the old name and both new names); the `summary` is on the old name's item. |
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
      "kind": "both_modified"
    }
  ],
  "next_steps": ["grit diff", "grit add ."],
  "alternatives": ["grit merge --abort"]
}
```

When `<rev>` names nothing, the command exits **1** with `{"error":"no branch, tag or commit named 'nonexist'","kind":"revision_not_found"}` on stdout (see [`revision_not_found`](../exit-codes/)).

When `--continue` finds unresolved conflicts, it exits **1** with `kind` `unmerged_paths`, the files in `paths` and the next step in `hint` (with `commit.autoStage=false`, files that are resolved but not staged get a `grit add` hint instead), for example `{"error":"you still have unmerged paths: notes.txt","hint":"…","kind":"unmerged_paths","paths":["notes.txt"]}`.

On overwrite refusal with `--json`, stdout is a single object with `kind` `would_overwrite` and `would_overwrite.reason` (`local_changes` or `untracked`) plus a sorted `paths` array, and a `hint` — the same shape as [`grit switch`](../switch/). For example `{"error":"local changes would be overwritten by merge","hint":"…","kind":"would_overwrite","would_overwrite":{"paths":["a.txt"],"reason":"local_changes"}}`. See the [error schema](/schemas/error/v1.json).

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
| `1` | Conflicts (`result: conflicts`), `<rev>` names nothing (`kind: revision_not_found`), a hook refused the merge commit (`kind: hook_failed`), or other failure ([`failure`](../exit-codes/)). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| `128` | `--ff-only` when not fast-forward ([`refused_not_ff`](../exit-codes/)). |

See [Exit codes](../exit-codes/).


## See also

[grit pull](../pull/), [grit pick](../pick/), [grit branch](../branch/)