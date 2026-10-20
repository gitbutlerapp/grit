---
title: grit log
summary: Show recent commits, newest first, a page at a time.
group: History
order: 1
---

## Synopsis

```text
grit log [<REV>…] [-n <N>] [--cursor <token>] [--stat]
  [--skip <N>] [--since|--after <date>] [--until|--before <date>]
  [--first-parent] [--merges | --no-merges] [--reverse]
  [--all] [--branches[=<glob>]] [--tags[=<glob>]] [--remotes[=<glob>]]
  [-- <path>…]
```

## Description

Lists commits reachable from the selected revisions (default `HEAD`), newest first, ten at a time (unless you pass `-n` / `--max-count`). Each line shows the short commit id, the author, when the commit was made and its subject (the first line of its message). The author is the commit's author name when present, otherwise the part of their email address before the `@`.

Revision selection matches [`grit revs`](../revs/): plain tips, `A..B`, `A...B`, `^tip`, and the same ref-set flags (`--all`, `--branches`, and so on). Path operands after `--` limit to commits that touch those paths (same cwd rules as [`grit diff`](../diff/)). Path filters use the same author/date output as the default log, not the bare oid list from `grit revs`. Without `-n`, every matching commit is listed when paths are set (not capped at ten).

When there's more history on a cursor-eligible walk, the last line shows the command for the next page. Path filters, `--reverse`, `--first-parent`, merge filters, `--skip`, and a one-shot `-n` without `--cursor` do not emit a continuation cursor.

With `--stat`, each commit is followed by the files it changed and how many lines were added and removed in each, like [`grit show`](../show/). Merge commits show the change against their first parent.

## Options

| Option | Description |
| --- | --- |
| `<REV>…` | Tips and ranges to walk (default `HEAD`). |
| `-n`, `--max-count <N>` | Stop after `N` commits. On the default walk, this is the page size (including when continuing with `--cursor`). With path filters, caps how many matching commits are returned; omit it to list all matches. A one-shot `-n` without `--cursor` and without paths lists `N` commits with no `next` cursor. `--limit` is a hidden alias for one release. |
| `--cursor <token>` | Continue a paged walk using the opaque `next` cursor from a previous `--json` page (or the `→ more` line). The cursor encodes the original revision selection, date cutoffs, and walk queue—pass `--cursor` alone (optional `-n` for page size). Revision operands, date filters, and ref-set flags are rejected with `--cursor`. Cannot be combined with path operands. |
| `--before <date>` | Alias for `--until` (date cutoff). Values starting with `lw1.` are still accepted as a deprecated continuation cursor (with a warning); use `--cursor` instead. |
| `--skip <N>` | Skip `N` commits after ordering (cursor walks do not support `--skip`). |
| `--since`, `--after <date>` | Exclude commits older than this date (committer date, Git parsing rules). |
| `--until`, `--before <date>` | Exclude commits newer than this date (unless `--before` is an `lw1.` cursor token). |
| `--first-parent` | Follow only the first parent at merges (disables cursor paging). |
| `--merges` | Only merge commits (conflicts with `--no-merges`; disables cursor paging). |
| `--no-merges` | Exclude merge commits (conflicts with `--merges`; disables cursor paging). |
| `--reverse` | Oldest commits first (disables cursor paging). |
| `--all` | Include every ref tip. |
| `--branches[=<glob>]` | Branch tips, optionally filtered by glob under `refs/heads/`. |
| `--tags[=<glob>]` | Tag tips, optionally filtered by glob under `refs/tags/`. |
| `--remotes[=<glob>]` | Remote-tracking tips, optionally filtered by glob under `refs/remotes/`. |
| `--stat` | Show which files each commit changed, with insertion and deletion counts. |
| `<path>…` | After `--`, limit to commits that touch these paths (cwd-relative under the work tree, like `grit diff`; all matches unless `-n` is set). |

## Examples

Show recent history:

```console
$ grit log
  b52cca6  Ada Lovelace  2 minutes ago  Handle empty input
  5fbefea  Ada Lovelace  1 hour ago     Add a greeting test
  ...
  2efcc2d  Ada Lovelace  3 days ago     Start the project

→ more: grit log --cursor=lw1.R0x…
```

(`lw1.` cursors are opaque; copy the full token from `--json` `next` when scripting.)

Show the next page:

```console
$ grit log --cursor=lw1.R0x…
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
$ grit log -n 5
```

History of one file:

```console
$ grit log -- src/lib.rs
```

Commits on a range:

```console
$ grit log -n 20 main..feature
```

## JSON output

Schema: [v1](/schemas/log/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Up to ten commits by default, or up to `-n` when set, newest first. Each entry has `oid`, `subject`, `author` (`name`, `email`, `date` in RFC 3339 — same shape as [`grit show`](../show/)), and `relative_date` (e.g. `3 days ago`). |
| `commits[].stat` | object | Only with `--stat`: `files` (each with `path`, `old_path` for renames, `status`, `insertions`, `deletions`, `binary`), `files_changed`, `insertions` and `deletions`. The same shape as `stat` in [`grit show`](../show/). |
| `next` | string or null | Opaque continuation cursor (prefix `lw1.`) to pass to `--cursor` for the next page, or `null` when there is no more history or when the walk does not support paging (paths, `--reverse`, and similar). |

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
      "oid": "919c45f33de5e0bd05f8ffb089f697f4644976",
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

More history: run `grit log --cursor=lw1.R0x…`
```

## See also

[grit revs](../revs/), [grit shortlog](../shortlog/), [grit show](../show/)
