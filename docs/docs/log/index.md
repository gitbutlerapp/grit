# grit log

> Show recent commits, newest first, a page at a time.

## Synopsis

```text
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

```console
$ grit log
  b52cca6  ada  2 minutes ago  Handle empty input
  5fbefea  ada  1 hour ago     Add a greeting test
  ...
  2efcc2d  ada  3 days ago     Start the project

→ more: grit log --before=a7e3a5e
```

Show the next page:

```console
$ grit log --before=a7e3a5e
```

Get the subjects of the last ten commits:

```console
$ grit log --json --filter '.commits[].subject'
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `commits` | array | Up to ten commits, newest first, each with `oid` and `subject`. |
| `next` | string or null | Full commit id to pass to `--before` for the next page, or `null` when there is no more history. |

Example:

```json
{
  "commits": [
    {
      "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
      "subject": "second"
    },
    {
      "oid": "919c45f33de5e5c0bd05f8ffb089f697f4644976",
      "subject": "initial"
    }
  ],
  "next": null
}
```

## See also

[grit shortlog](https://grit-scm.com/docs/shortlog/index.md), [grit show](https://grit-scm.com/docs/show/index.md)
