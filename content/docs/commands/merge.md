---
title: grit merge
summary: Merge another branch into the current one.
group: Branches and tags
order: 3
---

## Synopsis

```
grit merge <branch>
```

## Description

Brings the commits from `<branch>` into the current branch:

- If the current branch already has everything, nothing changes.
- If the current branch has no commits of its own since they diverged, it's moved forward to `<branch>` (a fast-forward). No new commit is made.
- Otherwise both sides are merged and a merge commit is recorded, with the message `Merge <branch>`.

`<branch>` can be a local branch or a remote-tracking branch such as `origin/main`. Local branches are looked up first.

If both sides changed the same lines, `grit` lists the conflicting files, exits with an error, and leaves the branch and working tree exactly as they were. `grit` can't resolve conflicts yet; to finish the merge, run `git merge <branch>`, fix the conflicts and commit.

`grit merge` won't run with uncommitted changes, on a branch with no commits, or with a detached HEAD.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to merge into the current one. |

## Examples

```
$ grit merge feature
Fast-forwarded feature → cf18394

$ grit merge origin/main
Merged origin/main into the current branch (acd1d4b)
```

When both branches change the same lines:

```
$ grit merge topic
error: merge has conflicts in:
  README.md

Nothing was changed. grit can't resolve conflicts yet — run `git merge topic` to resolve them.
```

## JSON output

```
{
  "result": "merged",
  "branch": "origin/main",
  "oid": "acd1d4b0c2f6e8a1b3d5c7e9f0a2b4c6d8e0f1a3"
}
```

| Field | Description |
| --- | --- |
| `result` | `up_to_date`, `fast_forward` or `merged`. |
| `branch` | The branch that was merged in. |
| `oid` | The commit the current branch now points at. |

## See also

[grit pull](../pull/), [grit pick](../pick/), [grit branch](../branch/)
