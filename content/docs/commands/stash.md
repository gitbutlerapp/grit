---
title: grit stash
summary: Save and restore shelved work.
group: Making changes
order: 4
---

## Synopsis

```text
grit stash [-m <msg>] [-u] [-- path…]
grit stash push [-m <msg>] [-u] [-S] [-k] [--hunks <id,...>] [--lines <path:start-end>] [--dry-run] [--pathspec-from-file <file|->] [--pathspec-file-nul] [-- path…]
grit stash list
grit stash show [<n>] [--stat] [-p] [-u]
grit stash apply [<n>] [--index]
grit stash pop [<n>] [--index]
grit stash branch <name> [<n>]
grit stash drop [<n>]
grit stash clear [--force] [--dry-run]
```

## Description

Shelve uncommitted changes and restore them later. With no subcommand, `grit stash` saves local changes (same as `grit stash push`), resets the working tree and index to `HEAD`, and records a stash entry compatible with Git's `refs/stash` format.

Like Git, stash never signs the commits it records, even when `commit.gpgsign` is true, so a configured signing program is not run on push.

Pass `--` followed by pathspecs (or `--pathspec-from-file`) on push to stash only matching paths; other staged and unstaged changes stay in place. The stash commit still records the full index on its index parent; the working-tree parent and optional untracked parent are limited to those paths, matching `git stash push -- <pathspec>`.

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
| `<path>…` | Limit what is stashed (after `--`); with `--staged`, only matching index changes are saved and reset. |
| `--pathspec-from-file` | Read pathspecs from a file, or `-` for stdin (same rules as Git). |
| `--pathspec-file-nul` | With `--pathspec-from-file`, entries are NUL-separated instead of line-based. |
| `--index` | On `apply` / `pop`, restore the stashed index as well as the worktree. |
| `<n>` | Stash index for `show`, `apply`, `pop`, or `drop` (default `0`). |
| `--stat` | Show a diffstat in `grit stash show` (default when `-p` is not set). |
| `-p`, `--patch` | Show the full patch in `grit stash show` instead of a diffstat. |
| `-u`, `--include-untracked` | On `show`, include untracked files from the stash U parent (same as `git stash show -u`). |
| `push` | Save changes explicitly (same flags as bare `grit stash`). |
| `list` | List stash entries. |
| `show` | Show a diffstat or patch for one entry. |
| `apply` | Apply an entry without removing it. |
| `pop` | Apply an entry and drop it when clean. |
| `drop` | Remove an entry without applying it. |
| `branch` | Create a branch at the stash base commit, check it out, apply with `--index`, and drop the entry on success. |
| `clear` | Remove every stash entry. Requires `--force`; without it the command exits non-zero and reports how many entries would be cleared. `--dry-run` reports the count without deleting. |

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

Schema: [v1](/schemas/stash/v1.json) · [stash.push v1](/schemas/stash.push/v1.json) · [stash.list v1](/schemas/stash.list/v1.json) · [stash.show v1](/schemas/stash.show/v1.json) · [stash.apply v1](/schemas/stash.apply/v1.json) · [stash.pop v1](/schemas/stash.pop/v1.json) · [stash.drop v1](/schemas/stash.drop/v1.json) · [stash.branch v1](/schemas/stash.branch/v1.json) · [stash.clear v1](/schemas/stash.clear/v1.json)

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
  "dry_run": true,
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
| `dry_run` | boolean | Always `true` on the preview object; omitted on successful save (`false`). |
| `paths` | array of strings | Tracked paths that would be stashed (`--dry-run` preview only). |
| `untracked` | array of strings | Untracked paths in a dry-run preview when `-u` is set. |
| `stashed_hunks` | object | Map of repository-relative path → hunk id strings stashed (partial `--hunks` only; same shape as `grit add --json`). Omitted when empty. |
| `stashed_lines` | object | Map of path → line range labels stashed (partial `--lines` only). Omitted when empty. |
| `paths` | array of strings | Resolved pathspecs when push was limited with `-- path…` or `--pathspec-from-file`. Omitted when empty. |
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
  "include_untracked": true,
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
| `include_untracked` | boolean | Whether `-u` / `--include-untracked` was used (omitted when false). |
| `stat` | object | Diffstat summary (when `-p` is not set). |
| `patch` | object | Full diff outcome (when `-p` is set). |

### `grit stash branch`

```json
{
  "branch": "from-stash",
  "base": "a1b2c3d4e5f6789012345678901234567890abcd",
  "index": 0,
  "conflicts": false
}
```

When `conflicts` is true, `conflict_paths` lists paths with unmerged index entries.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | Branch created and checked out. |
| `base` | string | Stash base commit (parent 1 of the W commit). |
| `index` | number | Stash index that was applied. |
| `conflicts` | boolean | Whether merge conflicts remain after apply. |
| `conflict_paths` | array of strings | Conflicted paths when `conflicts` is true. |

### `grit stash clear`

Without `--force` (preview only):

```json
{
  "cleared": 2,
  "dry_run": true
}
```

After `grit stash clear --force`:

```json
{
  "cleared": 2,
  "dry_run": false
}
```

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `cleared` | number | Stash reflog entries removed or that would be removed. |
| `dry_run` | boolean | True when nothing was deleted (`--force` missing or `--dry-run`). |

### `grit stash apply` / `grit stash pop`

```json
{
  "index": 0,
  "conflicts": false,
  "index_restored": false,
  "dropped": true
}
```

When `conflicts` is true, `conflict_paths` lists paths with unmerged index entries. On a clean apply or pop the field is omitted.

Pop adds `dropped` (whether the entry was removed):

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `index` | number | Stash index targeted. |
| `conflicts` | boolean | Whether merge conflicts remain. |
| `index_restored` | boolean | Whether `--index` was used. |
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



```text
## Stash

- Saved **On main: wip** · `690b7a6`
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit status](../status/), [grit restore](../restore/), [grit commit](../commit/)