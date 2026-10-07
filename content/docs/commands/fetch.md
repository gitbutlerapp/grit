---
title: grit fetch
summary: Download new commits, branches and tags from a remote.
group: Remotes
order: 2
---

## Synopsis

```
grit fetch [<remote>]
```

## Description

Downloads everything new from a remote and updates your remote-tracking branches, such as `origin/main`, along with any new tags. Your own branches and working tree are left alone; to bring the new commits into your branch, use [`grit merge`](../merge/) or [`grit pull`](../pull/).

Each updated ref is listed with its old and new commit.

## Options

| Option | Description |
| --- | --- |
| `<remote>` | The remote to fetch from. Defaults to `origin`. |

## Examples

```
$ grit fetch
  refs/remotes/origin/main  a8e620a → b52cca6
Fetched 1 update from origin.

$ grit fetch
Already up to date with origin.
```

Fetch from another remote:

```
$ grit fetch upstream
```

## JSON output

```
{
  "remote": "origin",
  "updates": [
    {
      "ref": "refs/remotes/origin/main",
      "old_oid": "a8e620a32cd86b483eaf4105fb215d4e920fcc14",
      "new_oid": "b52cca65378cf17309f0b1f917c6dc4fdd9256f2"
    }
  ],
  "updated": 1
}
```

`old_oid` is `null` for a ref that is new, and `new_oid` is `null` for one that was removed.

## See also

[grit pull](../pull/), [grit merge](../merge/), [grit remote](../remote/)
