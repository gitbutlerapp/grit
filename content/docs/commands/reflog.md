---
title: grit reflog
summary: Show, delete, or expire reflog entries.
group: History
order: 2
---

## Synopsis

```text
grit reflog [REF] [-n N] [--since DATE] [--until DATE] [--grep PAT]
grit reflog list [-n N]
grit reflog delete <REF@{N}>...
grit reflog expire [--all] [--expire <DATE>] [--expire-unreachable <DATE>] [--dry-run]
```

## Description

Inspect and maintain reference logs. With no subcommand, lists entries for `REF` (default `HEAD`) in newest-first order. Each line shows the selector (`REF@{n}`), the `new_oid` after the update, a relative date, and the log message.

Filters apply to the reflog entry identity timestamp (not the commit date). Scanning stops once `-n` matches are collected, so large logs are not fully materialized.

`delete` removes explicit entries without rewriting the branch tip. In `ref@{n}`, `n` is a nonnegative integer: `0` selects the newest entry, `1` the next older entry. Date selectors and bare indices are not accepted by `delete`. `list` prints every ref that currently has a reflog file (sorted), bounded by `-n` / `--limit`. `expire` applies Git-compatible `gc.reflogExpire*` rules (same layering as `grit gc`), using explicit `--expire` / `--expire-unreachable` values when given and merged config otherwise. With `--dry-run`, `expire` reports per-ref prune counts without rewriting any log file.

A branch literally named `list` is ambiguous with the `list` subcommand; use the full ref (`refs/heads/list`) when showing that branch's entries.

## Options

| Option | Description |
| --- | --- |
| `REF` | Ref to list (default `HEAD`). Branch short names are accepted (`main` → `refs/heads/main`). |
| `-n`, `--max-count` | Show at most `N` matching entries (newest first). Alias: `--limit`. |
| `--since`, `--after` | Exclude entries older than this date (Git `--since`). |
| `--until`, `--before` | Exclude entries newer than this date (Git `--until`). |
| `--grep` | Keep entries whose message contains this substring (case-sensitive). |
| `list` | List refs that have a reflog (`-n` / `--limit` bounds the list). |
| `delete` | Remove one or more `ref@{n}` selectors. |
| `expire` | Drop old reflog entries using gc expiry rules. |
| `--all` | On `expire`, walk every ref under `refs/`, not only refs that already have log files. |
| `--expire` | Reachable expiry time (`now`, `never`, `2.weeks.ago`, Unix time, …). |
| `--expire-unreachable` | Unreachable expiry time (same syntax as Git). |
| `--dry-run` | On `expire`, preview pruning without writing reflog files. |

## Examples

Show the last three `HEAD` reflog entries:

```console
$ grit reflog HEAD -n 3
HEAD@{0} b5ea30de417ac84c09b8cfc0acf5f2c39efc611f 2 minutes ago commit: second
HEAD@{1} 72934dedd47ecaf7e340fd6c5ef46620187fb317 1 hour ago commit: first
```

Entries since yesterday whose message mentions `checkout`:

```console
$ grit reflog --since=yesterday --grep=checkout -n 20 --json
```

Delete one entry:

```console
$ grit reflog delete main@{1}
Deleted main@{1}
```

Expire using gc defaults:

```console
$ grit reflog expire
Pruned 0 reflog entries
```

## JSON output

Schema: [v1](/schemas/reflog/v1.json) (show entries), [list v1](/schemas/reflog.list/v1.json), [delete v1](/schemas/reflog.delete/v1.json), [expire v1](/schemas/reflog.expire/v1.json)

List (`grit reflog [REF]`):

| Field | Type | Meaning |
| --- | --- | --- |
| `ref` | string | Ref that was listed. |
| `truncated` | boolean | `true` when listing stopped before the oldest entry (typically because of `-n`). |
| `entries` | array | Newest-first entries. |
| `entries[].index` | integer | Newest-first index in the full reflog (`0` is latest). |
| `entries[].oid` | string | Hex object id stored as `new_oid` for that entry (v1 alias of `new_oid`). |
| `entries[].old_oid` | string | Previous object id before the update. |
| `entries[].new_oid` | string | Object id after the update (same as `oid`). |
| `entries[].date` | string | Reflog committer time in RFC 3339 (original offset preserved). |
| `entries[].relative_date` | string | Human-relative time from the entry identity. |
| `entries[].committer` | object | `name`, `email`, and `date` (same shape as `log` / `blame`). |
| `entries[].message` | string | Reflog message (may be empty). |
| `entries[].action` | string (optional) | Best-effort prefix before the first `": "` in the message. |

```json
{
  "ref": "HEAD",
  "truncated": false,
  "entries": [
    {
      "index": 0,
      "oid": "b5ea30de417ac84c09b8cfc0acf5f2c39efc611f",
      "old_oid": "72934dedd47ecaf7e340fd6c5ef46620187fb317",
      "new_oid": "b5ea30de417ac84c09b8cfc0acf5f2c39efc611f",
      "date": "2026-10-23T18:15:00Z",
      "relative_date": "2 minutes ago",
      "committer": {
        "name": "Example User",
        "email": "user@example.com",
        "date": "2026-10-23T18:15:00Z"
      },
      "message": "commit: second",
      "action": "commit"
    }
  ]
}
```

Delete (`grit reflog delete …`):

| Field | Type | Meaning |
| --- | --- | --- |
| `deleted` | array of strings | Selectors that were removed. |

List refs (`grit reflog list`):

| Field | Type | Meaning |
| --- | --- | --- |
| `refs` | array | Sorted ref names that have a log. |
| `refs[].ref` | string | Full ref name (for example `refs/heads/main`). |

```json
{
  "refs": [
    { "ref": "HEAD" },
    { "ref": "refs/heads/main" }
  ]
}
```

Expire (`grit reflog expire`):

| Field | Type | Meaning |
| --- | --- | --- |
| `entries_pruned` | integer | Reflog lines removed (or that would be removed with `--dry-run`). |
| `dry_run` | boolean | `true` when `--dry-run` was passed (default `false`). |
| `refs` | array | Per-ref prune counts. |
| `refs[].ref` | string | Ref that was processed. |
| `refs[].pruned` | integer | Entries pruned on that ref. |

```json
{
  "entries_pruned": 0,
  "dry_run": false,
  "refs": []
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/reflog/v1.json) (show entries); subcommands use [list v1](/schemas/reflog.list/v1.json), [delete v1](/schemas/reflog.delete/v1.json), and [expire v1](/schemas/reflog.expire/v1.json).

List mode emits a bullet per entry with `REF@{n}`, abbreviated oid, **absolute** `date`, and message (human mode keeps relative dates). Delete and expire print short confirmation lines.




```text
- HEAD@{0} 83352a6 2023-11-14T22:13:20Z commit: second
- HEAD@{1} 062ca6e 2023-11-14T22:13:20Z commit (initial): first
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit log](log.md), [grit gc](gc.md)