---
title: grit merge
summary: Merge another branch into the current one.
group: Branches and tags
order: 3
---

## Synopsis

```text
grit merge <branch>
grit merge --abort
```

## Description

Brings the commits from `<branch>` into the current branch:

- If the current branch already has everything, nothing changes.
- If the current branch has no commits of its own since they diverged, it's moved forward to `<branch>` (a fast-forward). No new commit is made.
- Otherwise both sides are merged. When there are no overlapping edits, a merge commit is recorded with the message `Merge <branch>`. When `commit.gpgsign` is enabled, that merge commit is signed like [`grit commit`](../commit/).
- When the same lines were changed on both sides, `grit merge` stops with conflicts: it updates the index and working tree, writes conflict markers around each conflicting hunk, records merge-in-progress state (`MERGE_HEAD`, `ORIG_HEAD`, `MERGE_MSG`), and leaves the branch ref unchanged. Fix the conflicts and run [`grit commit`](../commit/) to create the two-parent merge commit (staging is automatic). Run `grit merge --abort` to discard the in-progress merge and restore the pre-merge branch, index, and working tree.

`<branch>` can be a local branch or a remote-tracking branch such as `origin/main`. Local branches are looked up first.

Local edits are allowed when they only touch paths the merge does not change, or when a changed path already matches the post-merge tree in the index (same scoped rule as [`grit switch`](../switch/)). Grit refuses when your changes would be overwritten by files the merge updates, when an untracked file would be replaced by a path the merge would check out, on a branch with no commits, or with a detached HEAD; the branch and file are left unchanged.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the current one. |
| `--abort` | Cancel an in-progress merge and restore the state recorded in `ORIG_HEAD`. |

## Examples

```console
$ grit merge feature
Fast-forwarded feature → cf18394

$ grit merge origin/main
Merged origin/main into the current branch (acd1d4b)
```

When both branches change the same lines:

```console
$ grit merge topic
CONFLICT (content): README.md

Automatic merge failed; fix conflicts and run `grit commit`, or run `grit merge --abort`.
```

When one branch deleted a file and the other modified it, grit leaves the modified side's content in the work tree (like Git) so you can run `grit add <path>` to keep it or `grit rm <path>` to accept the deletion:

```console
$ grit merge feat
CONFLICT (modify_delete): f deleted on the current branch and modified in feat; feat's version is in the work tree

Automatic merge failed; fix conflicts and run `grit commit`, or run `grit merge --abort`.
```

After resolving markers and staging:

```console
$ grit commit -m "Merge topic"
Merged topic (2 parents)
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

## JSON output

Schema: [v1](/schemas/merge/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, `conflicts`, `aborted`, or `set_upstream`. |
| `branch` | string | The branch that was merged in (or set as upstream). |
| `oid` | string or null | Commit the current branch points at after the merge, when applicable. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |
| `conflicts` | array or null | Sorted paths with unresolved conflicts when `result` is `conflicts`. |
| `conflict_details` | array or null | When `result` is `conflicts`, each item has `path`, `kind` (`content`, `add_add`, or `modify_delete`), and optional `summary` text. |

Example:

```json
{
  "result": "up_to_date",
  "branch": "main"
}
```

When the merge stops on conflicts (exit code 1), stdout is still a single JSON object:

```json
{
  "result": "conflicts",
  "branch": "topic",
  "conflicts": ["src/lib.rs"],
  "conflict_details": [
    {
      "path": "src/lib.rs",
      "kind": "content"
    }
  ]
}
```

## Markdown output

Schema: [v1](/schemas/merge/v1.json)
Pass [`--markdown`](../global-options/) for a `Merge` heading and a result summary. Fast-forward:

```text
- Fast-forwarded **`main`** → `9244cbd`
```

When the merge stops on conflicts (exit code 1), stdout still carries structured Markdown with conflict paths and next steps:

```text
- Merge **`side`** stopped with conflicts

- `f.txt`

→ fix conflicts, then `grit commit "message"`, or `grit merge --abort`
```

## See also

[grit pull](../pull/), [grit pick](../pick/), [grit branch](../branch/)
