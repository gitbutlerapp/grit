---
title: grit show
summary: Show a commit, tag or branch, and a summary of what it changed.
group: History
order: 3
---

## Synopsis

```text
grit show [<object>…] [--diff-merges first-parent|combined] [-- <path>…]
```

## Description

Shows a commit's id, author, date and full message, followed by the change that commit introduced (same delta-style patch as [`grit diff <commit>`](../diff/)). With no argument, it shows the current commit.

`<object>` can be a commit id (full or short), an expression like `HEAD~2`, a branch or a tag. For a branch or tag, the first line names it. For an annotated tag, the tag's own author, date and message come before the commit it points at.

**Merge commits** default to **combined** parent diffs (`--diff-merges=combined`, like `git show --cc`). Use `--diff-merges=first-parent` for a diff against the first parent only.

Pass several `<object>` values to show more than one commit. With `--json`, multiple objects are wrapped as `{ "objects": [ … ] }`; a single object keeps the usual top-level shape.

`revision:path` specs show blob contents or list tree entries (same resolution as [`grit cat`](cat.md)) without commit metadata.

Limit the commit diff to paths with `-- <path>…` after the object arguments.

## Options

| Option | Description |
| --- | --- |
| `<object>…` | Commits, branches, tags, or `rev:path` specs. Defaults to `HEAD`. |
| `--diff-merges` | `combined` (default) or `first-parent` for merge commits. |
| `-- <path>…` | Limit the diff to these paths (pathspecs). |

## Examples

Show the latest commit:

```console
$ grit show
branch feature
commit cf18394a62c3f845bd9c44927a5a55e014b2a99d
Author: Ada Lovelace <ada@example.com>
Date:   2026-10-07 10:00:00 +0000

    Say hi

 main.rs |   4 +++-
 1 file changed, 3 insertions(+), 1 deletion(-)
```

Show what a tag points at:

```console
$ grit show v0.1
```

Get the subject of a commit:

```console
$ grit show HEAD~1 --json --filter .commit.subject
"Start the project"
```

## JSON output

Schema: [v1](/schemas/show/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `warnings` | array | Non-fatal diagnostics emitted while resolving the object; omitted when empty. |
| `kind` | string | `commit`, `branch`, `tag`, or `annotated_tag`. |
| `ref_name` | string | Branch or tag name when you named a ref; omitted for a raw commit id. |
| `tag` | object | For an annotated tag: `name`, `tagger`, and `message`. |
| `commit` | object | Shared [`CommitJson`](../README.md#commit-json-object-commitjson): `oid`, `parents`, `author`, `committer`, `subject`, `message`, `refs`, optional `relative_date`. |
| `files` | array | Per-file diff rows (same shape as [`grit diff --json`](../diff/)). |
| `stat` | object | Change stats with `files`, `files_changed`, `insertions`, and `deletions`. |

Example:

```json
{
  "kind": "branch",
  "ref_name": "feature",
  "commit": {
    "oid": "92501f188ae0815af09a0cef6e121eddda4113cf",
    "parents": ["919c45f33de5e5c0bd05f8ffb089f697f4644976"],
    "author": {
      "name": "Ada Lovelace",
      "email": "ada@example.com",
      "date": "2026-10-07T14:54:02Z"
    },
    "committer": {
      "name": "Ada Lovelace",
      "email": "ada@example.com",
      "date": "2026-10-07T14:54:02Z"
    },
    "subject": "second",
    "message": "second",
    "refs": [
      { "name": "feature", "kind": "branch" },
      { "name": "HEAD", "kind": "head" }
    ],
    "relative_date": "2 days ago"
  },
  "stat": {
    "files": [
      {
        "path": "README.md",
        "status": "modified",
        "insertions": 1,
        "deletions": 0,
        "binary": false
      }
    ],
    "files_changed": 1,
    "insertions": 1,
    "deletions": 0
  }
}
```

## Markdown output

Schema: [v1](/schemas/show/v1.json)
Pass [`--markdown`](../global-options/) for headings, commit metadata, message body, and a diffstat table:

```text
# Commit `b52cca6`

**Oid:** `92501f188ae0815af09a0cef6e121eddda4113cf`
**Author:** Ada Lovelace <ada@example.com>
**Date:** 2026-10-07 14:54:02 +0000

Subject line as a level-2 heading, then a "Changes" section with a table:

| File | + | − |
| --- | ---: | ---: |
| `README.md` | 1 | 0 |

1 file changed, 1 insertion(+)
```

## See also

[grit diff](../diff/), [grit log](../log/), [grit tag](../tag/)
