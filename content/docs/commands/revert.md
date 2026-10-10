---
title: grit revert
summary: Undo a commit with a new inverse commit.
group: Branches and tags
order: 5
---

## Synopsis

```text
grit revert <commit>
```

## Description

Creates a new commit on the current branch that undoes the changes introduced by `<commit>`. The revert commit uses the standard `Revert "…"` subject and a `This reverts commit <oid>.` body, matching what Git writes with `git revert --no-edit`.

`<commit>` can be a full or short commit id, a branch name (to revert the commit at its tip) or an expression like `HEAD~1`.

`grit revert` works on one commit at a time. It refuses, and changes nothing, when:

- you have uncommitted changes, or the current branch has no commits, or HEAD is detached
- `<commit>` is a merge commit
- the revert would conflict with the current branch; the conflicting files are listed
- an untracked file would be replaced by a path the revert would check out (same rule as [`grit switch`](../switch/))

There is no range revert and no `--no-commit` mode.

## Options

| Option | Description |
| --- | --- |
| `<commit>` | The commit to undo on the current branch. |

## Examples

Revert the previous commit:

```console
$ grit revert HEAD~1
Reverted a1b2c3d → e4f5g6h Revert "Add feature"
```

Revert a commit by id:

```console
$ grit revert 9a1c2e0
```

When the revert conflicts:

```console
$ grit revert feature
revert has conflicts in:
  docs/guide.md

Nothing was changed. grit can't resolve conflicts yet — run `git revert feature` to resolve them.
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `source` | string | Full id of the commit that was reverted. |
| `oid` | string | Full id of the new revert commit on the current branch. |
| `subject` | string | Subject line of the revert commit (for example `Revert "Add feature"`). |

Example:

```json
{
  "source": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "oid": "a8f2c1d0e9b876543210abcdef0123456789abcd",
  "subject": "Revert \"Add feature\""
}
```

## See also

[grit pick](../pick/), [grit merge](../merge/)
