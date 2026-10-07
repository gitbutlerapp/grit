---
title: grit show
summary: Show a commit, tag or branch, and a summary of what it changed.
group: History
order: 3
---

## Synopsis

```
grit show [<object>]
```

## Description

Shows a commit's id, author, date and full message, followed by a summary of the files it changed and how many lines were added and removed in each. With no argument, it shows the current commit.

`<object>` can be a commit id (full or short), an expression like `HEAD~2`, a branch or a tag. For a branch or tag, the first line names it. For an annotated tag, the tag's own author, date and message come before the commit it points at.

To see the full change a commit made, use [`grit diff <commit>`](../diff/).

## Options

| Option | Description |
| --- | --- |
| `<object>` | The commit, branch or tag to show. Defaults to the current commit. |

## Examples

Show the latest commit:

```
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

```
$ grit show v0.1
```

Get the subject of a commit:

```
$ grit show HEAD~1 --json --filter .commit.subject
"Start the project"
```

## JSON output

```
{
  "kind": "branch",
  "ref_name": "feature",
  "commit": {
    "oid": "cf18394a62c3f845bd9c44927a5a55e014b2a99d",
    "parents": ["217c6f958c3162926c9c9379e6d21b38f110e1ad"],
    "author": { "name": "Ada Lovelace", "email": "ada@example.com", "date": "2026-10-07 10:00:00 +0000" },
    "committer": { "name": "Ada Lovelace", "email": "ada@example.com", "date": "2026-10-07 10:00:00 +0000" },
    "subject": "Say hi",
    "message": "Say hi"
  },
  "stat": {
    "files": [
      { "path": "main.rs", "status": "modified", "insertions": 3, "deletions": 1, "binary": false }
    ],
    "files_changed": 1,
    "insertions": 3,
    "deletions": 1
  }
}
```

| Field | Description |
| --- | --- |
| `kind` | `commit`, `branch`, `tag` or `annotated_tag`. |
| `ref_name` | The branch or tag name. Absent when you named a commit directly. |
| `tag` | For an annotated tag: its `name`, `tagger` (`name`, `email`, `date`) and `message`. |
| `commit` | The commit's id, parent ids, author, committer, subject and full message. |
| `stat.files[]` | Each changed file, with its `status` and line counts. A renamed file also has `old_path`. |

## See also

[grit diff](../diff/), [grit log](../log/), [grit tag](../tag/)
