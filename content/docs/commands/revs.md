---
title: grit revs
summary: List or count commits matching revision ranges.
group: History
order: 3
---

## Synopsis

```text
grit revs <RANGE>... [-n <N>] [--count] [--left-right]
  [--skip <N>] [--since|--after <date>] [--until|--before <date>]
  [--first-parent] [--merges | --no-merges] [--reverse]
  [--all] [--branches[=<glob>]] [--tags[=<glob>]] [--remotes[=<glob>]]
  [-- <PATH>...]
```

## Description

Walks commits reachable from one or more revision ranges and prints each commit’s full object id and subject (first line of the message), one commit per line. Ranges use the same syntax as Git’s `rev-list`: plain tips, `A..B`, symmetric difference `A...B`, and `^tip` exclusions.

Use `--count` to print only the number of reachable commits (no subject lines). With `--left-right` and exactly one `A...B` range, `--count` prints left and right counts (same as `git rev-list --left-right --count`). When a commit-graph file is present, counting uses it for speed.

Path arguments after `--` limit history to commits that touched those paths. Operands use Git pathspec cwd rules (same as [`grit diff`](../diff/)): from a subdirectory, `.` limits to that directory, and bare filenames are relative to the current directory, not the repository root.

## Options

| Option | Description |
| --- | --- |
| `<RANGE>…` | One or more revision ranges (required unless a ref-set flag such as `--all` is used). |
| `-n`, `--max-count <N>` | Stop after `N` commits (listing mode only). `--limit` is a hidden alias for one release. |
| `--count` | Print only the commit count (or left/right counts with `--left-right`). |
| `--left-right` | With `--count` and one `A...B` range, print `{left, right}` instead of a single total. |
| `--skip <N>` | Skip `N` commits after ordering. |
| `--since`, `--after <date>` | Exclude commits older than this date (committer date). |
| `--until`, `--before <date>` | Exclude commits newer than this date. |
| `--first-parent` | Follow first parents only at merges. |
| `--merges` | Only merge commits (conflicts with `--no-merges`). |
| `--no-merges` | Exclude merge commits (conflicts with `--merges`). |
| `--reverse` | Oldest commits first. |
| `--all` | Include every ref tip. |
| `--branches[=<glob>]` | Branch tips, optionally filtered by glob. |
| `--tags[=<glob>]` | Tag tips, optionally filtered by glob. |
| `--remotes[=<glob>]` | Remote-tracking tips, optionally filtered by glob. |
| `-- <PATH>…` | Limit to commits touching these paths (cwd-relative under the work tree, like `grit diff`). |

## Examples

List commits on the current branch:

```console
$ grit revs HEAD
92501f188ae0815af09a0cef6e121eddda4113cf  second
919c45f33de5e0bd05f8ffb089f697f4644976  initial
```

Count reachable commits:

```console
$ grit revs --count HEAD
2
```

Left/right counts on a symmetric range:

```console
$ grit revs --count --left-right main...feature
3	5
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
| `commits` | array | Commits in walk order. Each entry uses the shared [`CommitJson`](../schemas/#commit-json-object-commitjson) shape. Empty when `--count` is set. |
| `count` | number or null | Total reachable commits when `--count` is set without `--left-right`; otherwise `null`. |
| `left_right` | object or omitted | When `--count` and `--left-right` are set: `left` and `right` commit counts for the symmetric range. |
| `truncated` | boolean | `true` when more commits matched than returned (`-n`). |
| `next` | null | Reserved for future paging; always `null` today. |

Example (listing):

```json
{
  "commits": [
    {
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "subject": "second"
    }
  ],
  "count": null,
  "truncated": false,
  "next": null
}
```

Example (`--count`):

```json
{
  "commits": [],
  "count": 2
}
```

Example (`--count --left-right`):

```json
{
  "commits": [],
  "count": null,
  "left_right": {
    "left": 3,
    "right": 5
  }
}
```

## Markdown output

Schema: [v1](/schemas/revs/v1.json)

Pass [`--markdown`](../global-options/) for a heading and bullet list of oid/subject pairs, or a bold count when `--count` is used.

## See also

[grit log](../log/), [grit merge-base](../merge-base/), [grit shortlog](../shortlog/)
