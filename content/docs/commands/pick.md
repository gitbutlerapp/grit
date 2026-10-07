---
title: grit pick
summary: Copy a single commit onto the current branch.
group: Branches and tags
order: 4
---

## Synopsis

```
grit pick <commit>
```

## Description

Takes the change `<commit>` made and applies it to the current branch as a new commit (a cherry-pick). The new commit keeps the original author and message.

`<commit>` can be a full or short commit id, a branch name (to pick the commit at its tip) or an expression like `feature~2`.

`grit pick` works on one commit at a time and keeps it simple. It refuses, and changes nothing, when:

- you have uncommitted changes, or the current branch has no commits, or HEAD is detached
- `<commit>` is a merge commit
- `<commit>` doesn't change anything, or its change is already on the current branch
- the change conflicts with the current branch; the conflicting files are listed

## Options

| Option | Description |
| --- | --- |
| `<commit>` | The commit to copy onto the current branch. |

## Examples

Pick the latest commit from another branch:

```
$ grit pick feature
Picked cf18394 → 4be20a1 Say hi
```

Pick a commit by id:

```
$ grit pick 9a1c2e0
```

## JSON output

```
{
  "source": "cf18394a62c3f845bd9c44927a5a55e014b2a99d",
  "oid": "4be20a1f3c5e7d9b0a2c4e6f8a1b3d5c7e9f0a2b",
  "subject": "Say hi"
}
```

`source` is the commit that was picked and `oid` is the new commit on the current branch.

## See also

[grit merge](../merge/), [grit log](../log/)
