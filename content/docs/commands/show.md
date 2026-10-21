---
title: grit show
summary: Show a commit, tag or branch, and the change it introduced.
group: History
order: 3
---

## Synopsis

```text
grit show [<object>…] [--diff-merges first-parent|combined] [--stat | --numstat | --shortstat | --name-only | --name-status] [-U[n]] [-W] [-- <path>…]
```

## Description

Shows a commit's id, author, date and full message, followed by the change that commit introduced (delta-style patch by default, like `grit diff <commit>`). With no argument, it shows the current commit.

`<object>` can be a commit id (full or short), an expression like `HEAD~2`, a branch or a tag. For a branch or tag, the first line names it. For an annotated tag, the tag's own author, date and message come before the commit it points at.

**Merge commits** default to **combined** parent diffs (`--diff-merges=combined`, like `git show --cc`). Use `--diff-merges=first-parent` for a diff against the first parent only.

Pass several `<object>` values to show more than one commit (human and markdown output are concatenated). With `--json`, multiple objects are wrapped as `{ "objects": [ … ] }`; a single object keeps the usual top-level shape.

`revision:path` specs (same resolution as [`grit cat`](cat.md)) show blob contents or list tree entries without commit metadata.

Use `--stat` (or other diff format flags below) instead of the default patch. Limit output to paths with `-- <path>…` after the commit arguments.

## Options

| Option | Description |
| --- | --- |
| `<object>…` | Commits, branches, tags, or `rev:path` specs. Defaults to `HEAD`. |
| `--diff-merges` | `combined` (default) or `first-parent` for merge commits. |
| `-p`, `--patch` | Full patch (the default). |
| `--patch-text` | Accepted for CLI parity with [`grit diff`](diff.md); use `grit diff --patch-text` for git-apply output. |
| `--stat` | Summary with change bars per file instead of a full patch. |
| `--numstat` | Machine-readable `insertions<TAB>deletions<TAB>path` lines. |
| `--shortstat` | Only the summary line (`N files changed, …`). |
| `--name-only` | Changed paths, one per line. |
| `--name-status` | Status letter and path(s) per line. |
| `-p`, `--patch` | Unified patch (default when no other format flag is set). |
| `-U`, `--unified <n>` | Lines of context around each change in patch mode (bare `-U` means 3). |
| `-W`, `--function-context` | Expand hunks to enclosing function boundaries (patch mode). |
| `-p`, `--patch` | Unified patch output (default when no other format flag is set). |
| `-- <path>…` | Limit the diff to these paths (pathspecs). |

## Examples

Show the latest commit with patch (default):

```console
$ grit show
branch feature
commit cf18394a62c3f845bd9c44927a5a55e014b2a99d
Author: Ada Lovelace <ada@example.com>
Date:   2026-10-07 10:00:00 +0000

    Say hi

## main.rs
**status:** modified

- 1  1
+ 1  2
 context line
- old line
+ new line
```

Show only the diffstat (previous default style):

```console
$ grit show HEAD --stat
```

Show one file from a commit:

```console
$ grit show HEAD~1 -- README.md
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
| `objects` | array | When more than one `<object>` is requested: each element is a commit show object or a `rev:path` blob/tree object (below). |
| `warnings` | array | Non-fatal diagnostics emitted while resolving the object; omitted when empty. |
| `kind` | string | For a commit, branch, tag, or annotated tag: `commit`, `branch`, `tag`, or `annotated_tag`. For `rev:path`: `blob` or `tree`. |
| `ref_name` | string | Branch or tag name when you named a ref; omitted for a raw commit id. |
| `tag` | object | For an annotated tag: `name`, `tagger`, and `message`. |
| `commit` | object | Commit id, parents, author, committer, subject, and full message. |
| `files` | array | Per-file diff rows (same shape as [`grit diff`](diff.md) `--json`), including optional `combined_status` (`MM`, `DD`, …) on combined merge name-status output. |
| `totals` | object | Aggregate line counts when a stat-style format is selected; omitted for patch-only output unless the library computed totals. |
| `stat` | object | Summary diffstat with `files`, `files_changed`, `insertions`, and `deletions` (always present on commit shows). |
| `spec` | string | For `rev:path`: the object spec you passed (for example `HEAD:README.md`). |
| `path` | string | For `rev:path`: repository-relative path after the first revision colon. |
| `oid` | string | For `rev:path`: object id of the blob or tree. |
| `size` | integer | For `rev:path`: uncompressed object size in bytes. |
| `text` | string | For a text blob at `rev:path`; omitted for binary blobs. |
| `data_base64` | string | For a binary blob at `rev:path`; standard base64 of raw bytes. |
| `binary` | boolean | For `rev:path` blobs: true when `data_base64` is used instead of `text`. |
| `entries` | array | For `rev:path` trees: `mode`, `type`, `oid`, and `path` per entry. |

Combined merge hunks may attach a `parents` array on each patch line in `files[].hunks[].lines[]`.

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
    "message": "second"
  },
  "files": [
    {
      "path": "README.md",
      "status": "modified",
      "binary": false,
      "hunks": []
    }
  ],
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

Pass [`--markdown`](../global-options/) for headings, commit metadata, message body, and fenced `diff` blocks per file (or a diffstat table with `--stat`):

```text
# Commit `b52cca6`

**Oid:** `92501f188ae0815af09a0cef6e121eddda4113cf`
**Author:** Ada Lovelace <ada@example.com>
**Date:** 2026-10-07T14:54:02Z

## second

~~~diff
@@ -1,1 +1,2 @@
 line
+added
~~~
```

## See also

- [`grit diff`](diff.md) — compare commits, ranges, or the working tree without commit metadata.
- [`grit log`](log.md) — list recent commits.
