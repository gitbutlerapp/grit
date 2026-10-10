---
title: grit shortlog
summary: List the commits on this branch that aren't on the target branch yet.
group: History
order: 2
---

## Synopsis

```text
grit shortlog
grit sl
```

## Description

Shows the current branch, its target branch, and every commit on the current branch that the target doesn't have, newest first. It's the list of what you would be proposing if you opened a pull request now.

The target branch is found the same way as for [`grit status`](../status/#the-target-branch): the `target.branch` setting, then `origin/master`, `origin/main`, `master` and `main`. Unlike `grit status`, which shows at most ten commits, `grit shortlog` lists all of them.

## Options

`grit shortlog` takes no options beyond the [global ones](../global-options/).

## Examples

```console
$ grit shortlog
On feature
Ahead of origin/main by 2 commits
  cf18394  ada  2 hours ago  Say hi
  9a1c2e0  ada  3 hours ago  Add a greeting test
```

Count the commits that aren't on the target yet:

```console
$ grit sl --json --filter .ahead
2
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Current branch name. |
| `target` | string or null | Target branch used for comparison, or `null` when none was found. |
| `ahead` | number | How many commits are on the branch but not on the target. |
| `commits` | array | Those commits, newest first, each with `oid` and `subject`. |

Example:

```json
{
  "branch": "feature",
  "target": "main",
  "ahead": 0,
  "commits": []
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for a branch summary and commit bullets:

```text
# Branch `feature`

Ahead of `main` by **2** commits.

- `cf18394` Say hi (ada, 1 hour ago)
- `9a1c2e0` Add a greeting test (ada, 2 hours ago)
```

## See also

[grit status](../status/), [grit log](../log/), [grit config](../config/)
