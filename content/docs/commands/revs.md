---
title: grit revs
summary: List or count commits matching revision ranges.
group: History
order: 3
---

## Synopsis

```text
grit revs <RANGE>... [-n <N>] [--count] [--left-right] [--cherry-mark]
  [--skip <N>] [--since|--after <date>] [--until|--before <date>]
  [--first-parent] [--follow] [--merges | --no-merges] [--reverse]
  [--author <pat>] [--committer <pat>] [--grep <pat>]
  [-S <string>] [-G <pat>] [--pickaxe-regex]
  [-i] [--all-match] [--invert-grep] [-F] [--no-mailmap]
  [--all] [--branches[=<glob>]] [--tags[=<glob>]] [--remotes[=<glob>]]
  [--pathspec-from-file <file|->] [--pathspec-file-nul]
  [-- <PATH>...]
```

## Description

Walks commits reachable from one or more revision ranges and prints each commit’s full object id and subject (first line of the message), one commit per line. Ranges use the same syntax as Git’s `rev-list`: plain tips, `A..B`, symmetric difference `A...B`, and `^tip` exclusions.

Use `--count` to print only the number of reachable commits (no subject lines). With `--left-right` and exactly one `A...B` range, `--count` prints left and right counts (same as `git rev-list --left-right --count`). Without `--count`, `--left-right` lists the range with a `<` (left) or `>` (right) prefix on each commit.

### Reviewing a rewritten branch

`--cherry-mark` compares the two sides of one `A...B` range by [patch-ID](../patch-id/), the way `git rev-list --left-right --cherry-mark` does. Run it on the version you reviewed and the version that was force-pushed (`old...new`) to see which commits are the same change and which are new:

- `=` marks a commit whose patch-ID also appears on the other side. When exactly one commit on each side has that patch-ID, the line names its counterpart and says whether the commit message changed, so a reworded commit stands out from an untouched one.
- `<` and `>` mark commits found only on the left or right side: dropped, added, or edited commits, and commits that came in with the new base.

When several commits on one side share a patch-ID, they are all marked equivalent without being paired. Matching uses the stable patch-ID (path-limited when paths follow `--`). Only the diff counts: a commit whose content changed is a new commit on its side, and no content interdiff is shown. `--cherry-mark` and `--left-right` need exactly one `A...B` range and exit **1** otherwise, even with `-n 0`; `--cherry-mark` cannot be combined with `--count`. When a commit-graph file is present, the walk takes parents and commit dates from it instead of reading commit objects. Unfiltered counts use that graph linkage, and listings read each listed commit once to render its row.

`A..B` and `A...B` counts walk the range once: the excluded side is marked down
to where it meets the included side, and the walk stops there instead of
building ancestor sets for the shared history. `--since`, `--until`,
`--merges`, `--no-merges` and `--first-parent` only decide which of those
commits are counted, so counting a range costs about the same as listing it.
With a commit-graph, generation numbers let the walk stop as soon as the
included side is resolved, so a small range stays bounded even in a repository
with a deep history. As in Git, `--first-parent` follows first parents from the
included tips only; everything an excluded tip reaches through any parent stays
excluded.

Path arguments after `--` limit history to commits that touched those paths. Operands use Git pathspec cwd rules (same as [`grit diff`](../diff/)): from a subdirectory, `.` limits to that directory, and bare filenames are relative to the current directory, not the repository root.

Author, committer, message, and pickaxe filters use the same rules as [`grit log`](../log/) (`-S`, `-G`, `--pickaxe-regex`, and the same `-i` / `-F` / mailmap behavior). Invalid patterns exit **129** with `kind: "invalid_pattern"` in `--json` output.

## Options

| Option | Description |
| --- | --- |
| `<RANGE>…` | One or more revision ranges (required unless a ref-set flag such as `--all` is used). |
| `-n`, `--limit <N>` | Stop after `N` commits (listing mode only). `--max-count` is Git’s well-known alias. |
| `--count` | Print only the commit count (or left/right counts with `--left-right`). |
| `--left-right` | With one `A...B` range, mark each listed commit as left (`<`) or right (`>`); with `--count`, print `{left, right}` instead of a single total. |
| `--cherry-mark` | With one `A...B` range, pair the two sides by patch-ID: `=` marks equivalent commits, `<` / `>` the rest (implies `--left-right`; conflicts with `--count`). |
| `--skip <N>` | Skip `N` commits after ordering. |
| `--since`, `--after <date>` | Exclude commits older than this date (committer date). |
| `--until`, `--before <date>` | Exclude commits newer than this date. |
| `--first-parent` | Follow first parents only at merges. |
| `--follow` | With exactly one literal file path after `--`, walk history across renames (Git `log --follow`). |
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

What changed in a force-pushed branch since review (`old-feature` is the reviewed version; the branch was rebased onto a new `main` commit, `c3` was reworded and `c4` was added):

```console
$ grit revs --cherry-mark old-feature...feature
= 09d5318d2c9bb2881fd2b503861aadf43737463e  c3  (message changed, pairs with 05075c9436c1)
> 42f209db819ee73ae750d5d33a85a4a972c75c36  c4 edits f1
= 05075c9436c17c49302513b5960c332ecfcdbae4  c3 reworded  (message changed, pairs with 09d5318d2c9b)
= 9000bf12a1499df8fc46332773bfd822e31dbcbd  c2  (pairs with a41d2d81bcbe)
= 8d35e4e808b5b12bdd0910e7fbc0eaad0804d1d4  c1  (pairs with e778fe04f497)
> 4c78873b7c8588803452c16634f5b36ae4164264  m
= a41d2d81bcbea60b67ac7f6c530d71bea889b628  c2  (pairs with 9000bf12a149)
= e778fe04f49705176ea24d7fa28182952a835187  c1  (pairs with 8d35e4e808b5)
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
| `commits` | array | Commits in walk order. Each entry uses the shared [`CommitJson`](../README.md#commit-json-object-commitjson) shape, plus the side fields below with `--left-right` or `--cherry-mark`. Empty when `--count` is set. |
| `commits[].side` | string or omitted | `left` or `right`: which side of the `A...B` range the commit is on. Only with `--left-right` or `--cherry-mark`. |
| `commits[].equivalent` | boolean or omitted | With `--cherry-mark`: `true` when a commit on the other side has the same patch-ID. |
| `commits[].equivalent_to` | string or omitted | With `--cherry-mark`, when exactly one commit on each side shares the patch-ID: the other side's commit. |
| `commits[].message_changed` | boolean or omitted | Present with `equivalent_to`: `true` when the two commit messages differ. |
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

Example (`--cherry-mark -n 1 old-feature...feature`):

```json
{
  "commits": [
    {
      "oid": "09d5318d2c9bb2881fd2b503861aadf43737463e",
      "parents": [
        "a41d2d81bcbea60b67ac7f6c530d71bea889b628"
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
      "subject": "c3",
      "message": "c3",
      "refs": [
        {
          "name": "old-feature",
          "kind": "branch"
        }
      ],
      "relative_date": "[TIME_AGO]",
      "change_id": "sntxlnlnxkouurtynzpmvokryyvmmqwv",
      "change_id_stored": false,
      "side": "left",
      "equivalent": true,
      "equivalent_to": "05075c9436c17c49302513b5960c332ecfcdbae4",
      "message_changed": true
    }
  ],
  "count": null,
  "truncated": true,
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

With `--left-right` or `--cherry-mark`, each bullet starts with the side in bold, and equivalent pairs carry a note:

```text
# Commits

- **left, equivalent** `09d5318d2c9bb2881fd2b503861aadf43737463e` c3 _(message changed, pairs with 05075c9436c1)_
- **right** `42f209db819ee73ae750d5d33a85a4a972c75c36` c4 edits f1
- **right, equivalent** `05075c9436c17c49302513b5960c332ecfcdbae4` c3 reworded _(message changed, pairs with 09d5318d2c9b)_
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit log](../log/), [grit merge-base](../merge-base/), [grit patch-id](../patch-id/), [grit shortlog](../shortlog/)