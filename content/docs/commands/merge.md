---
title: grit merge
summary: Merge another branch into the current one.
group: Branches and tags
order: 3
---

## Synopsis

```text
grit merge <branch>
```

## Description

Brings the commits from `<branch>` into the current branch:

- If the current branch already has everything, nothing changes.
- If the current branch has no commits of its own since they diverged, it's moved forward to `<branch>` (a fast-forward). No new commit is made.
- Otherwise both sides are merged and a merge commit is recorded, with the message `Merge <branch>`. When `commit.gpgsign` is enabled, that merge commit is signed like [`grit commit`](../commit/).

`<branch>` can be a local branch or a remote-tracking branch such as `origin/main`. Local branches are looked up first.

If both sides changed the same lines, `grit` lists the conflicting files, exits with an error, and leaves the branch and working tree exactly as they were. `grit` can't resolve conflicts yet; to finish the merge, run `git merge <branch>`, fix the conflicts and commit.

`grit merge` won't run with uncommitted changes, on a branch with no commits, or with a detached HEAD. It also refuses when an untracked file in the working tree would be replaced by a path the merge would check out (same rule as [`grit switch`](../switch/)); the branch and file are left unchanged.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the current one. |

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
error: merge has conflicts in:
  README.md

Nothing was changed. grit can't resolve conflicts yet — run `git merge topic` to resolve them.
```

When an untracked file would be overwritten:

```console
$ grit merge feature
error: untracked file 'notes.txt' would be overwritten — move or remove it first
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `result` | string | `up_to_date`, `fast_forward`, `merged`, or `set_upstream`. |
| `branch` | string | The branch that was merged in (or set as upstream). |
| `oid` | string or null | Commit the current branch points at after the merge, when applicable. |
| `upstream` | string or null | For `set_upstream`, the remote-tracking branch that was adopted. |

Example:

```json
{
  "result": "up_to_date",
  "branch": "main"
}
```

## See also

[grit pull](../pull/), [grit pick](../pick/), [grit branch](../branch/)
