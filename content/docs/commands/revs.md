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
  [--author <pat>] [--committer <pat>] [--grep <pat>]
  [-S <string>] [-G <pat>] [--pickaxe-regex]
  [-i] [--all-match] [--invert-grep] [-F] [--no-mailmap]
  [--all] [--branches[=<glob>]] [--tags[=<glob>]] [--remotes[=<glob>]]
  [--pathspec-from-file <file|->] [--pathspec-file-nul]
  [-- <PATH>...]
```

## Description

Walks commits reachable from one or more revision ranges and prints each commit’s full object id and subject (first line of the message), one commit per line. Ranges use the same syntax as Git’s `rev-list`: plain tips, `A..B`, symmetric difference `A...B`, and `^tip` exclusions.

Use `--count` to print only the number of reachable commits (no subject lines). With `--left-right` and exactly one `A...B` range, `--count` prints left and right counts (same as `git rev-list --left-right --count`). When a commit-graph file is present, the walk takes parents and commit dates from it instead of reading commit objects: counting reads no commits at all, and listing reads each listed commit once, only to render its row.

Path arguments after `--` limit history to commits that touched those paths. Operands use Git pathspec cwd rules (same as [`grit diff`](../diff/)): from a subdirectory, `.` limits to that directory, and bare filenames are relative to the current directory, not the repository root.

Author, committer, message, and pickaxe filters use the same rules as [`grit log`](../log/) (`-S`, `-G`, `--pickaxe-regex`, and the same `-i` / `-F` / mailmap behavior). Invalid patterns exit **129** with `kind: "invalid_pattern"` in `--json` output.

## Options

| Option | Description |
| --- | --- |
| `<RANGE>…` | One or more revision ranges (required unless a ref-set flag such as `--all` is used). |
| `-n`, `--limit <N>` | Stop after `N` commits (listing mode only). `--max-count` is Git’s well-known alias. |
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
| `--author <pat>` | Only commits whose author matches (repeatable; OR). |
| `--committer <pat>` | Only commits whose committer matches (repeatable; OR). |
| `--grep <pat>` | Only commits whose message matches (repeatable; OR unless `--all-match`). |
| `-i`, `--regexp-ignore-case` | Case-insensitive patterns. |
| `--all-match` | Every `--grep` pattern must match. |
| `--invert-grep` | Exclude commits that match the grep group. |
| `-F`, `--fixed-strings` | Literal string patterns. |
| `--no-mailmap` | Do not apply `.mailmap` for author/committer matching. |
| `--pathspec-from-file <file\|->` | Read pathspecs from a file (`-` for stdin), in addition to argv paths after `--`. |
| `--pathspec-file-nul` | `--pathspec-from-file` entries are NUL-separated. |
| `-- <PATH>…` | Limit to commits touching these paths (cwd-relative under the work tree, like `grit diff`). |

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

Left/right counts on a symmetric range:

```console
$ grit revs --count --left-right main...feature
3	5
```

Commits on one branch but not another:

```console
$ grit revs main..feature
```

Count commits mentioning an issue id on a range:

```console
$ grit revs --count --grep=JIRA-123 main..
```

## JSON output

Schema: [v1](/schemas/revs/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Commits in walk order. Each entry uses the shared [`CommitJson`](../README.md#commit-json-object-commitjson) shape. Empty when `--count` is set. |
| `count` | number or null | Total reachable commits when `--count` is set without `--left-right`; otherwise `null`. |
| `left_right` | object or omitted | When `--count` and `--left-right` are set: `left` and `right` commit counts for the symmetric range. |
| `truncated` | boolean | `true` when more commits matched than returned (`-n`). |
| `next` | null | Reserved for future paging; always `null` today. |

Example (listing):

```json
{
  "commits": [
    {
      "oid": "83352a61f6715ec3f0a297c6d6150725154fb833",
      "parents": [
        "062ca6e74f5603f4d09063c0f46cd07d8c96e019"
      ],
      "author": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "committer": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "subject": "second",
      "message": "second",
      "refs": [
        {
          "name": "HEAD",
          "kind": "head"
        },
        {
          "name": "main",
          "kind": "branch"
        }
      ],
      "relative_date": "[TIME_AGO]",
      "change_id": "xzyqzyyzswyupvtlsovyzultryuqzlvk",
      "change_id_stored": true
    },
    {
      "oid": "062ca6e74f5603f4d09063c0f46cd07d8c96e019",
      "parents": [],
      "author": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "committer": {
        "name": "Test",
        "email": "test@example.com",
        "date": "2023-11-14T22:13:20Z"
      },
      "subject": "first",
      "message": "first",
      "refs": [
        {
          "name": "feature",
          "kind": "branch"
        }
      ],
      "relative_date": "[TIME_AGO]",
      "change_id": "prkwwuwrytvxrwozplzrxmvozwutplkq",
      "change_id_stored": true
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
  "count": 2,
  "truncated": false,
  "next": null
}
```

Example (`--count --left-right`):

```json
{
  "commits": [],
  "count": null,
  "truncated": false,
  "next": null,
  "left_right": {
    "left": 3,
    "right": 5
  }
}
```

## Markdown output

Schema: [v1](/schemas/revs/v1.json)

Pass [`--markdown`](../global-options/) for a heading and bullet list of oid/subject pairs, or a bold count when `--count` is used.




```text
# Commits

- `83352a61f6715ec3f0a297c6d6150725154fb833` second
- `062ca6e74f5603f4d09063c0f46cd07d8c96e019` first
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit log](../log/), [grit merge-base](../merge-base/), [grit shortlog](../shortlog/)