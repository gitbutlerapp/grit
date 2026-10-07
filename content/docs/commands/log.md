---
title: grit log
summary: Show recent commits, newest first, a page at a time.
group: History
order: 1
---

## Synopsis

```
grit log [--before <commit>]
```

## Description

Lists the commits reachable from the current commit, newest first, ten at a time. Each line shows the short commit id, the author, when the commit was made and its subject (the first line of its message). The author is shown by the part of their email address before the `@`, or by name when there's no email.

When there's more history, the last line shows the command for the next page.

## Options

| Option | Description |
| --- | --- |
| `--before <commit>` | Start listing at this commit instead of the current one. Use the commit from the `→ more` line to see the next page. |

## Examples

Show recent history:

```
$ grit log
  b52cca6  ada  2 minutes ago  Handle empty input
  5fbefea  ada  1 hour ago     Add a greeting test
  ...
  2efcc2d  ada  3 days ago     Start the project

→ more: grit log --before=a7e3a5e
```

Show the next page:

```
$ grit log --before=a7e3a5e
```

Get the subjects of the last ten commits:

```
$ grit log --json --filter '.commits[].subject'
```

## JSON output

```
{
  "commits": [
    { "oid": "cf18394a62c3f845bd9c44927a5a55e014b2a99d", "subject": "Say hi" },
    { "oid": "217c6f958c3162926c9c9379e6d21b38f110e1ad", "subject": "Start the project" }
  ],
  "next": null
}
```

`next` is the full id to pass to `--before` for the next page, or `null` when there is no more history.

## See also

[grit shortlog](../shortlog/), [grit show](../show/)
