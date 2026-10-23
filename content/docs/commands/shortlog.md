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

For author commit counts over an arbitrary revision range (Git's `shortlog -sne` shape), use [`grit log --group-by author`](log.md#options) instead — `grit shortlog` means commits ahead of the target branch, not author grouping.

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

Schema: [v1](/schemas/shortlog/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Current branch name. |
| `target` | string or null | Target branch used for comparison, or `null` when none was found. |
| `ahead` | number | How many commits are on the branch but not on the target. |
| `commits` | array | Those commits, newest first, each using the shared [`CommitJson`](../README.md#commit-json-object-commitjson) shape (bounded list; see `truncated`). |
| `truncated` | boolean | `true` when `ahead` exceeds the number of commits listed. |
| `next` | null | Reserved for future paging; always `null` today. |

Example:

```json
{
  "branch": "main",
  "target": "main",
  "ahead": 0,
  "commits": [],
  "truncated": false,
  "next": null
}
```

## Markdown output

Schema: [v1](/schemas/shortlog/v1.json)
Pass [`--markdown`](../global-options/) for a branch summary and commit bullets:

```text
# Branch `feature`

Ahead of `main` by **2** commits.

- `cf18394` Say hi (ada, 1 hour ago)
- `9a1c2e0` Add a greeting test (ada, 2 hours ago)
```


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit status](../status/), [grit log](../log/), [grit config](../config/)