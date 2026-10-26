---
title: grit shortlog
summary: List the commits on this branch that aren't on the target branch yet.
group: History
order: 2
---

## Synopsis

```text
grit shortlog [-n COUNT] [--behind] [--stat | --name-only] [--target REF]
grit sl [-n COUNT] [--behind] [--stat | --name-only] [--target REF]
grit ahead [-n COUNT] [--behind] [--stat | --name-only] [--target REF]
```

## Description

Shows the current branch, its target branch, and every commit on the current branch that the target doesn't have, newest first. It's the list of what you would be proposing if you opened a pull request now.

The target branch is found the same way as for [`grit status`](../status/#the-target-branch): the `target.branch` setting, then `origin/master`, `origin/main`, `master` and `main`. Use `--target <REF>` to compare against any ref or revision instead (branch, remote-tracking ref, tag, or commit id). `grit ahead` is an alias for the same command — grit's shortlog means commits ahead of a target, not Git's author-grouped `shortlog -sne`. Unlike `grit status`, which shows at most ten commits, `grit shortlog` lists up to `-n` / `--max-count` commits (default **100**). Full ahead and behind **counts** are always exact even when the lists are truncated.

When no target branch can be discovered, the result is an empty list. An explicit `--target` that cannot be resolved is an error.

With `--behind`, also lists commits that are on the target but not on this branch yet (the commits you would pick up by fast-forwarding or merging the target).

For author commit counts over an arbitrary revision range (Git's `shortlog -sne` shape), use [`grit log --group-by author`](log.md#options) instead — `grit shortlog` means commits ahead of the target branch, not author grouping.

## Options

| Flag | Meaning |
| ---- | ------- |
| `--target <REF>` | Ref or revision to compare against instead of the discovered target branch. |
| `-n`, `--max-count` | Maximum commits to list on each side (default 100). Counts stay exact when the list is truncated. |
| `--behind` | Also list commits on the target that are not on this branch (`behind` / `behind_commits` in JSON). |
| `--stat` | Per-commit diffstat (same shape as [`grit log --stat`](log.md)). |
| `--name-only` | Per-commit changed paths (same shape as [`grit log --name-only`](log.md)). |

Only `--stat` and `--name-only` are supported for per-commit diffs; use [`grit log`](log.md) for patch and other formats.

Global options: [global options](../global-options/).

## Examples

```console
$ grit shortlog
On feature
Ahead of origin/main by 2 commits
  cf18394  ada  2 hours ago  Say hi
  9a1c2e0  ada  3 hours ago  Add a greeting test
```

List at most one ahead commit and show how far behind the target you are:

```console
$ grit sl -n 1 --behind --json --filter '{ahead, behind, truncated, behind_truncated, commits: .commits|length, behind_commits: .behind_commits|length}'
```

Per-commit stats for the ahead list:

```console
$ grit shortlog --stat
```

## JSON output

Schema: [v1](/schemas/shortlog/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Current branch name. |
| `target` | string or null | Target branch used for comparison, or `null` when none was found. |
| `ahead` | number | How many commits are on the branch but not on the target. |
| `behind` | number or null | How many commits are on the target but not on the branch; `null` when no target was found. |
| `commits` | array | Ahead commits, newest first, each using the shared [`CommitJson`](../README.md#commit-json-object-commitjson) shape plus optional `stat` / `files` when `--stat` / `--name-only` is set. |
| `truncated` | boolean | `true` when `ahead` exceeds the number of commits listed. |
| `behind_commits` | array or null | Behind commits when `--behind` is set; same commit object shape as `commits`. |
| `behind_truncated` | boolean or null | `true` when `behind` exceeds `behind_commits` length; only with `--behind`. |
| `next` | null | Reserved for future paging; always `null` today. |

Example:

```json
{
  "branch": "main",
  "target": "main",
  "ahead": 2,
  "behind": 0,
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

See [Exit codes](../exit-codes/) for the shared rules and structured `--json` `kind` values.


## See also

[grit status](../status/), [grit log](../log/), [grit config](../config/)
