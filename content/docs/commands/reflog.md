---
title: grit reflog
summary: Show, delete, or expire reflog entries.
group: History
order: 2
---

## Synopsis

```text
grit reflog [REF] [--limit N]
grit reflog list [-n N]
grit reflog delete <REF@{N}>...
grit reflog expire [--all] [--expire <DATE>] [--expire-unreachable <DATE>] [--dry-run]
```

## Description

Inspect and maintain reference logs. With no subcommand, lists entries for `REF` (default `HEAD`) in newest-first order. Each line shows the selector (`REF@{n}`), the `new_oid` after the update, a relative date, and the log message.

`delete` removes explicit entries without rewriting the branch tip. `list` prints every ref that currently has a reflog file (sorted), bounded by `-n` / `--limit`. `expire` applies Git-compatible `gc.reflogExpire*` rules (same layering as `grit gc`), using explicit `--expire` / `--expire-unreachable` values when given and merged config otherwise. With `--dry-run`, `expire` reports per-ref prune counts without rewriting any log file.

A branch literally named `list` is ambiguous with the `list` subcommand; use the full ref (`refs/heads/list`) when showing that branch's entries.

## Options

| Option | Description |
| --- | --- |
| `REF` | Ref to list (default `HEAD`). Branch short names are accepted (`main` → `refs/heads/main`). |
| `--limit` | Show at most `N` entries (newest first). |
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
$ grit reflog HEAD --limit 3
HEAD@{0} b5ea30de417ac84c09b8cfc0acf5f2c39efc611f 2 minutes ago commit: second
HEAD@{1} 72934dedd47ecaf7e340fd6c5ef46620187fb317 1 hour ago commit: first
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
| `entries` | array | Newest-first entries. |
| `entries[].index` | integer | Newest-first index (`0` is latest). |
| `entries[].oid` | string | Hex object id stored as `new_oid` for that entry. |
| `entries[].relative_date` | string | Human-relative time from the entry identity. |
| `entries[].message` | string | Reflog message (may be empty). |

```json
{
  "ref": "HEAD",
  "entries": [
    {
      "index": 0,
      "oid": "b5ea30de417ac84c09b8cfc0acf5f2c39efc611f",
      "relative_date": "2 minutes ago",
      "message": "commit: second"
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

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/reflog/v1.json) (show entries); subcommands use [list v1](/schemas/reflog.list/v1.json), [delete v1](/schemas/reflog.delete/v1.json), and [expire v1](/schemas/reflog.expire/v1.json).

List mode emits a bullet per entry with the same `REF@{n} oid relative-date message` fields. Delete and expire print short confirmation lines.

## See also

[grit log](log.md), [grit gc](gc.md)
