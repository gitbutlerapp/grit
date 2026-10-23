---
title: grit stash
summary: Save and restore shelved work.
group: Making changes
order: 4
---

## Synopsis

```text
grit stash [-m <msg>] [-u]
grit stash push [-m <msg>] [-u] [-S] [-k] [--hunks <id,...>] [--lines <path:start-end>] [--dry-run]
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

When a stash was created with `-u` / `--include-untracked`, `apply` and `pop` refuse to write stashed untracked files onto a path that already exists on disk (visible untracked, ignored, or tracked content you added after stashing). The command exits non-zero, the stash entry is kept, and your existing file bytes are unchanged.

## Options

| Option | Description |
| --- | --- |
| `-m`, `--message` | Message stored on the stash reflog (push / bare `grit stash`). |
| `-u`, `--include-untracked` | Include untracked files in the stash (not with `--hunks` / `--lines` or `--staged`). |
| `-S`, `--staged` | Stash only index changes vs `HEAD`; unstaged edits on other paths remain (not with `-u`, `--hunks`, or `--lines`). |
| `-k`, `--keep-index` | Stash tracked changes but leave the index and worktree matching staged content afterward. |
| `--hunks` | Stash only listed unstaged hunk ids from `grit diff --unstaged --json`. |
| `--lines` | Stash only unstaged line ranges (`path:start-end`, same as `grit add --lines`). |
| `--dry-run` | Preview what would be stashed; no objects, refs, index, or worktree updates. |
| `-n`, `--limit` | Cap path lists in `--dry-run` preview output. |
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
Saved working directory and index state On main: wip feature a1b2c3d
```

List entries:

```console
$ grit stash list
stash@{0} a1b2c3d: On main: wip feature
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

When pop would overwrite an existing path from a `-u` stash:

```console
$ grit stash pop
stash untracked restore blocked: 'notes.txt' already exists on disk
```

## JSON output

Schema: [v1](/schemas/stash/v1.json) · [stash.push v1](/schemas/stash.push/v1.json) · [stash.list v1](/schemas/stash.list/v1.json) · [stash.show v1](/schemas/stash.show/v1.json) · [stash.apply v1](/schemas/stash.apply/v1.json) · [stash.pop v1](/schemas/stash.pop/v1.json) · [stash.drop v1](/schemas/stash.drop/v1.json)

Pass `--json` for stable, scripting-friendly output.

### `grit stash` / `grit stash push`

When something was stashed:

```json
{
  "oid": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
  "message": "On main: wip feature",
  "mode": "all"
}
```

Partial stash (`--hunks` / `--lines`) also includes `stashed_hunks` and `stashed_lines` maps (same shape as `grit add --json`).

`--dry-run` preview (no `oid`):

```json
{
  "mode": "staged_only",
  "paths": ["staged_new.txt"],
  "untracked": []
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
| `oid` | string | Full object id of the stash commit (success object only). |
| `message` | string | Reflog message (for `-m`, `On <branch>: …`; otherwise Git-style `WIP on …`). |
| `mode` | string | `all`, `staged_only`, or `keep_index`. Omitted when `all`. |
| `paths` | array of strings | Tracked paths that would be stashed (`--dry-run` preview only). |
| `untracked` | array of strings | Untracked paths in a dry-run preview when `-u` is set. |
| `stashed_hunks` | object | Map of repository-relative path → hunk id strings stashed (partial `--hunks` only; same shape as `grit add --json`). Omitted when empty. |
| `stashed_lines` | object | Map of path → line range labels stashed (partial `--lines` only). Omitted when empty. |
| `stashed` | boolean | Present and `false` only on the empty-worktree result. |

### `grit stash list`

The JSON value is a top-level array (newest entry first):

```json
[
  {
    "index": 0,
    "oid": "6e513d5cd6ab8d618238e22d7ca55d942b55964a",
    "message": "On main: second"
  }
]
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `index` | number | Index matching `stash@{index}`. |
| `oid` | string | Full stash commit id. |
| `message` | string | Reflog message. |

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

With `--patch`, a `patch` object with the same shape as [`grit diff`](../diff/) is included instead of `stat`.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `index` | number | Stash index shown. |
| `stat` | object | Diffstat summary (when `-p` is not set). |
| `patch` | object | Full diff outcome (when `-p` is set). |

### `grit stash apply` / `grit stash pop`

```json
{
  "index": 0,
  "conflicts": false
}
```

When `conflicts` is true, `conflict_paths` lists paths with unmerged index entries. On a clean apply or pop the field is omitted.

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

Schema: [v1](/schemas/stash/v1.json)
With `--markdown`, list/apply/pop/drop use bullet lists; `show` renders the diffstat or patch like other diff commands.

## See also

[grit status](../status/), [grit restore](../restore/), [grit commit](../commit/)
