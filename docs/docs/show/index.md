# grit show

> Show a commit, tag or branch, and a summary of what it changed.

## Synopsis

```
grit show [<object>]
```

## Description

Shows a commit's id, author, date and full message, followed by a summary of the files it changed and how many lines were added and removed in each. With no argument, it shows the current commit.

`<object>` can be a commit id (full or short), an expression like `HEAD~2`, a branch or a tag. For a branch or tag, the first line names it. For an annotated tag, the tag's own author, date and message come before the commit it points at.

To see the full change a commit made, use [`grit diff <commit>`](https://grit-scm.com/docs/diff/index.md).

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

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `kind` | string | `commit`, `branch`, `tag`, or `annotated_tag`. |
| `ref_name` | string | Branch or tag name when you named a ref; omitted for a raw commit id. |
| `tag` | object | For an annotated tag: `name`, `tagger`, and `message`. |
| `commit` | object | Commit id, parents, author, committer, subject, and full message. |
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
      "date": "2026-10-07 14:54:02 +0000"
    },
    "committer": {
      "name": "Ada Lovelace",
      "email": "ada@example.com",
      "date": "2026-10-07 14:54:02 +0000"
    },
    "subject": "second",
    "message": "second"
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

## See also

[grit diff](https://grit-scm.com/docs/diff/index.md), [grit log](https://grit-scm.com/docs/log/index.md), [grit tag](https://grit-scm.com/docs/tag/index.md)
