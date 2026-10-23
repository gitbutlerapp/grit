---
title: grit reflog
summary: Show, delete, or expire reflog entries.
group: History
order: 2
---

## Synopsis

```text
grit reflog [REF] [--limit N]
grit reflog delete <REF@{N}>...
grit reflog expire [--all] [--expire <DATE>] [--expire-unreachable <DATE>]
```

## Description

Inspect and maintain reference logs. With no subcommand, lists entries for `REF` (default `HEAD`) in newest-first order. Each line shows the selector (`REF@{n}`), the `new_oid` after the update, a relative date, and the log message.

`delete` removes explicit entries without rewriting the branch tip. `expire` applies Git-compatible `gc.reflogExpire*` rules (same layering as `grit gc`), using explicit `--expire` / `--expire-unreachable` values when given and merged config otherwise.

## Options

| Option | Description |
| --- | --- |
| `REF` | Ref to list (default `HEAD`). Branch short names are accepted (`main` → `refs/heads/main`). |
| `--limit` | Show at most `N` entries (newest first). |
| `delete` | Remove one or more `ref@{n}` selectors. |
| `expire` | Drop old reflog entries using gc expiry rules. |
| `--all` | On `expire`, walk every ref under `refs/`, not only refs that already have log files. |
| `--expire` | Reachable expiry time (`now`, `never`, `2.weeks.ago`, Unix time, …). |
| `--expire-unreachable` | Unreachable expiry time (same syntax as Git). |

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

Schema: [v1](/schemas/reflog/v1.json) (list), [delete v1](/schemas/reflog.delete/v1.json), [expire v1](/schemas/reflog.expire/v1.json)

List (`grit reflog [REF]`):

| Field | Type | Meaning |
| --- | --- | --- |
| `ref` | string | Ref that was listed. |
| `entries` | array | Newest-first entries. |
| `entries[].index` | integer | Newest-first index (`0` is latest). |
| `entries[].oid` | string | Hex object id stored as `new_oid` for that entry (v1 alias of `new_oid`). |
| `entries[].old_oid` | string | Previous object id before the update. |
| `entries[].new_oid` | string | Object id after the update (same as `oid`). |
| `entries[].date` | string | Reflog committer time in RFC 3339 (original offset preserved). |
| `entries[].relative_date` | string | Human-relative time from the entry identity. |
| `entries[].committer` | object | `name`, `email`, and `date` (same shape as `log` / `blame`). |
| `entries[].message` | string | Reflog message (may be empty). |

```json
{
  "ref": "HEAD",
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
      "message": "commit: second"
    }
  ]
}
```

Delete (`grit reflog delete …`):

| Field | Type | Meaning |
| --- | --- | --- |
| `deleted` | array of strings | Selectors that were removed. |

Expire (`grit reflog expire`):

| Field | Type | Meaning |
| --- | --- | --- |
| `entries_pruned` | integer | Reflog lines removed. |

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/reflog/v1.json) (list); subcommands use [delete v1](/schemas/reflog.delete/v1.json) and [expire v1](/schemas/reflog.expire/v1.json).

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