---
title: grit log
summary: Show recent commits, newest first, a page at a time.
group: History
order: 1
---

## Synopsis

```text
grit log [--before <cursor>] [--limit <N>] [-p] [--stat | --numstat | --shortstat | --name-only | --name-status] [-U<N>] [-- <path>…]
```

## Description

Lists the commits reachable from the current commit, newest first, ten at a time (unless you pass `--limit`). Each line shows the short commit id, the author, when the commit was made and its subject (the first line of its message). The author is the commit's author name when present, otherwise the part of their email address before the `@`.

Pass one or more paths after `--` to show only commits that touched those paths (same rules as [`grit revs`](../revs/)). Path operands follow Git pathspec cwd rules—the same as [`grit diff`](../diff/): from a subdirectory, `f.txt` means that file under your current directory, not at the repository root. Path filters use the same author/date output as the default log, not the bare oid list from `grit revs`. Without `--limit`, every matching commit is listed (not capped at ten).

When there's more history on the default walk, the last line shows the command for the next page. Path filters and a one-shot `--limit` without `--before` do not emit a continuation cursor.

Diff detail flags (same family as [`grit diff`](../diff/)) append output under each commit line: `-p` / `--patch` (unified patch), `--stat`, `--numstat`, `--shortstat`, `--name-only`, or `--name-status`. Only one format at a time. `-U` sets unified context lines for patches (default from `diff.context`).

With `--stat`, each commit is also summarized in JSON under `stat` (legacy shape matching [`grit show`](../show/)). Merge commits show the change against their first parent for stat and name modes; `-p` skips merge patches (like `git log -p`). Path operands limit both which commits appear and the paths included in each patch.

## Options

| Option | Description |
| --- | --- |
| `--before <cursor>` | Continue a paged walk using the opaque `next` cursor from a previous `--json` page (or the `→ more` line). This is not a commit oid: it encodes the walk queue so merges and side branches are not skipped. Cannot be combined with path operands. |
| `--limit <N>` | Stop after `N` commits. On the default walk, this is the page size (including when continuing with `--before`). With path filters, caps how many matching commits are returned; omit it to list all matches. A one-shot `--limit` without `--before` and without paths lists `N` commits with no `next` cursor. |
| `-p`, `--patch` | Unified patch under each commit (merge commits omit the patch). |
| `--stat` | Per-file change bars and totals under each commit. |
| `--numstat` | Tab-separated insertion/deletion/path lines per commit. |
| `--shortstat` | One summary line per commit. |
| `--name-only` | Changed paths only, one per line per commit. |
| `--name-status` | Status letter and path(s) per line per commit. |
| `-U`, `--unified <N>` | Context lines for `-p` (Git `-U`). |
| `<path>…` | After `--`, limit to commits that touch these paths (cwd-relative under the work tree, like `grit diff`; all matches unless `--limit` is set). |

## Examples

Show recent history:

```console
$ grit log
  b52cca6  Ada Lovelace  2 minutes ago  Handle empty input
  5fbefea  Ada Lovelace  1 hour ago     Add a greeting test
  ...
  2efcc2d  Ada Lovelace  3 days ago     Start the project

→ more: grit log --before=lw1.R0x…
```

(`lw1.` cursors are opaque; copy the full token from `--json` `next` when scripting.)

Show the next page:

```console
$ grit log --before=lw1.R0x…
```

See which files each recent commit touched:

```console
$ grit log --stat
  b52cca6  ada  2 minutes ago  Handle empty input

 src/parse.rs | 12 +++++++++---
 1 file changed, 9 insertions(+), 3 deletions(-)

  5fbefea  ada  1 hour ago     Add a greeting test
  ...
```

Get the subjects of the last ten commits:

```console
$ grit log --json --filter '.commits[].subject'
```

Show the five newest commits:

```console
$ grit log --limit 5
```

History of one file:

```console
$ grit log -- src/lib.rs
```

## JSON output

Schema: [v1](/schemas/log/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Up to ten commits by default, or up to `--limit` when set, newest first. Each entry has `oid`, `subject`, `author` (`name`, `email`, `date` in RFC 3339 — same shape as [`grit show`](../show/)), and `relative_date` (e.g. `3 days ago`). |
| `commits[].files` | array | With any diff detail flag: same [`FileDiff`](/schemas/diff/v1.json) objects as [`grit diff`](../diff/) (including `stat` sub-objects for `--stat` / `--numstat`). |
| `commits[].totals` | object | With stat-style flags: `files_changed`, `insertions`, `deletions`. |
| `commits[].stat` | object | Only with `--stat`: duplicate summary in the legacy [`grit show`](../show/) `stat` shape (`files`, `files_changed`, …). |
| `next` | string or null | Opaque continuation cursor (prefix `lw1.`) to pass to `--before` for the next page, or `null` when there is no more history. |

Example:

```json
{
  "commits": [
    {
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "subject": "second",
      "author": {
        "name": "Ada Lovelace",
        "email": "ada@example.com",
        "date": "2026-10-07T14:54:02Z"
      },
      "relative_date": "2 days ago"
    },
    {
      "oid": "919c45f33de5e5c0bd05f8ffb089f697f4644976",
      "subject": "initial",
      "author": {
        "name": "Ada Lovelace",
        "email": "ada@example.com",
        "date": "2026-10-05T09:12:00Z"
      },
      "relative_date": "4 days ago"
    }
  ],
  "next": null
}
```

## Markdown output

Schema: [v1](/schemas/log/v1.json)
Pass [`--markdown`](../global-options/) for a bullet list of commits (short oid, subject, author, relative date):

```text
- `b52cca6` Handle empty input (Ada Lovelace, 2 minutes ago)
- `5fbefea` Add a greeting test (Ada Lovelace, 1 hour ago)

More history: run `grit log --before=lw1.R0x…`
```

## See also

[grit shortlog](../shortlog/), [grit show](../show/)
