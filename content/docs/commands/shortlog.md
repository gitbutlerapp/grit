---
title: grit shortlog
summary: List the commits on this branch that aren't on the target branch yet.
group: History
order: 2
---

## Synopsis

```
grit shortlog
grit sl
```

## Description

Shows the current branch, its target branch, and every commit on the current branch that the target doesn't have, newest first. It's the list of what you would be proposing if you opened a pull request now.

The target branch is found the same way as for [`grit status`](../status/#the-target-branch): the `target.branch` setting, then `origin/master`, `origin/main`, `master` and `main`. Unlike `grit status`, which shows at most ten commits, `grit shortlog` lists all of them.

## Options

`grit shortlog` takes no options beyond the [global ones](../#options-for-every-command).

## Examples

```
$ grit shortlog
On feature
Ahead of origin/main by 2 commits
  cf18394  ada  2 hours ago  Say hi
  9a1c2e0  ada  3 hours ago  Add a greeting test
```

Count the commits that aren't on the target yet:

```
$ grit sl --json --filter .ahead
2
```

## JSON output

```
{
  "branch": "feature",
  "target": "origin/main",
  "ahead": 2,
  "commits": [
    { "oid": "cf18394a62c3f845bd9c44927a5a55e014b2a99d", "subject": "Say hi" },
    { "oid": "9a1c2e0d4b6f8a1c3e5d7f9b0a2c4e6d8f0a1b3c", "subject": "Add a greeting test" }
  ]
}
```

`target` is `null` when no target branch was found.

## See also

[grit status](../status/), [grit log](../log/), [grit config](../config/)
