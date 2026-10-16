---
title: grit revs
summary: List or count commits matching revision ranges.
group: History
order: 3
---

## Synopsis

```text
grit revs <RANGE>... [--count] [--limit N] [--first-parent] [-- <PATH>...]
```

## Description

Walks commits reachable from one or more revision ranges and prints each commit’s full object id and subject (first line of the message), one commit per line. Ranges use the same syntax as Git’s `rev-list`: plain tips, `A..B`, symmetric difference `A...B`, and `^tip` exclusions.

Use `--count` to print only the number of reachable commits (no subject lines). When a commit-graph file is present, counting uses it for speed.

Path arguments after `--` limit history to commits that touched those paths.

## Options

| Option | Description |
| --- | --- |
| `<RANGE>…` | One or more revision ranges (required). |
| `--count` | Print only the commit count. |
| `--limit N` | Stop after `N` commits (listing mode only). |
| `--first-parent` | Follow first parents only at merges. |
| `-- <PATH>…` | Limit to commits touching these paths. |

## Examples

List commits on the current branch:

```console
$ grit revs HEAD
92501f188ae0815af09a0cef6e121eddda4113cf  second
919c45f33de5e5c0bd05f8ffb089f697f4644976  initial
```

Count reachable commits:

```console
$ grit revs --count HEAD
2
```

Commits on one branch but not another:

```console
$ grit revs main..feature
```

## JSON output

Schema: [v1](/schemas/revs/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Commits in walk order. Each entry has `oid` and `subject`. Empty when `--count` is set. |
| `count` | number or null | Total reachable commits when `--count` is set; otherwise `null`. |

Example (listing):

```json
{
  "commits": [
    {
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "subject": "second"
    }
  ],
  "count": null
}
```

Example (`--count`):

```json
{
  "commits": [],
  "count": 2
}
```

## Markdown output

Schema: [v1](/schemas/revs/v1.json)

Pass [`--markdown`](../global-options/) for a heading and bullet list of oid/subject pairs, or a bold count when `--count` is used.

## See also

[grit log](../log/), [grit merge-base](../merge-base/), [grit shortlog](../shortlog/)
