---
title: grit stash
summary: Save and restore shelved work.
group: Making changes
order: 4
---

## Synopsis

```text
grit stash [-m <msg>] [-u]
grit stash push [-m <msg>] [-u]
grit stash list
grit stash show [<n>] [-p]
grit stash apply [<n>]
grit stash pop [<n>]
grit stash drop [<n>]
```

## Description

Shelve uncommitted changes and restore them later. With no subcommand, `grit stash` saves local changes (same as `grit stash push`), resets the working tree and index to `HEAD`, and records a stash entry compatible with Git's `refs/stash` format.

Stash indices follow Git: `0` is the newest entry. You can pass an integer or `stash@{n}`.

`grit stash pop` applies the entry and removes it when there are no conflicts. When applying would leave merge conflicts, the stash entry is kept and the command exits non-zero; conflicting paths are listed.

## Options

| Option | Description |
| --- | --- |
| `-m`, `--message` | Message stored on the stash reflog (push / bare `grit stash`). |
| `-u`, `--include-untracked` | Include untracked files in the stash. |
| `<n>` | Stash index for `show`, `apply`, `pop`, or `drop` (default `0`). |
| `-p`, `--patch` | Show the full patch in `grit stash show` instead of a diffstat. |
| `push` | Save changes explicitly (same flags as bare `grit stash`). |
| `list` | List stash entries. |
| `show` | Show a diffstat or patch for one entry. |
| `apply` | Apply an entry without removing it. |
| `pop` | Apply an entry and drop it when clean. |
| `drop` | Remove an entry without applying it. |

## Examples

Save work in progress:

```console
$ grit stash -m "wip feature"
Saved working directory and index state On wip feature a1b2c3d
```

List entries:

```console
$ grit stash list
stash@{0} a1b2c3d: WIP on main: wip feature
```

Show the newest stash as a diffstat:

```console
$ grit stash show
 f.txt | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)
```

Pop the newest entry:

```console
$ grit stash pop
Dropped stash@{0} (applied)
```

When pop leaves conflicts:

```console
$ grit stash pop
Stash pop left conflicts in:
  f.txt
The stash entry was kept.
```

## JSON output

Pass `--json` for stable, scripting-friendly output.

### `grit stash` / `grit stash push`

When something was stashed:

```json
{
  "oid": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "message": "WIP on main: wip feature",
  "stashed": true
}
```

When there was nothing to save:

```json
{
  "stashed": false
}
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `stashed` | boolean | Whether a new stash entry was created. |
| `oid` | string | Full object id of the stash commit (when `stashed` is true). |
| `message` | string | Reflog message Git would show after the colon (when `stashed` is true). |

### `grit stash list`

```json
{
  "entries": [
    {
      "index": 0,
      "oid": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
      "message": "WIP on main: second"
    }
  ]
}
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `entries` | array | Stash entries, newest first. |
| `entries[].index` | number | Index matching `stash@{index}`. |
| `entries[].oid` | string | Full stash commit id. |
| `entries[].message` | string | Reflog message. |

### `grit stash show`

Diffstat (default):

```json
{
  "index": 0,
  "stat": {
    "files_changed": 1,
    "insertions": 1,
    "deletions": 1,
    "files": [
      {
        "path": "f.txt",
        "status": "modified",
        "insertions": 1,
        "deletions": 1,
        "binary": false
      }
    ]
  }
}
```

With `--patch`, a `patch` object with the same shape as [`grit diff`](diff/) is included instead of `stat`.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `index` | number | Stash index shown. |
| `stat` | object | Diffstat summary (when `-p` is not set). |
| `patch` | object | Full diff outcome (when `-p` is set). |

### `grit stash apply` / `grit stash pop`

```json
{
  "index": 0,
  "conflicts": false,
  "conflict_paths": []
}
```

Pop adds `dropped` (whether the entry was removed):

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `index` | number | Stash index targeted. |
| `conflicts` | boolean | Whether merge conflicts remain. |
| `conflict_paths` | array of strings | Paths with unmerged index entries when `conflicts` is true. |
| `dropped` | boolean | (`pop` only) Whether the stash entry was removed. |

### `grit stash drop`

```json
{
  "index": 0
}
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `index` | number | Stash index removed. |

## Markdown output

With `--markdown`, list/apply/pop/drop use bullet lists; `show` renders the diffstat or patch like other diff commands.

## See also

[grit status](../status/), [grit restore](../restore/), [grit commit](../commit/)
