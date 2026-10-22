---
title: grit branch
summary: List branches, or create, delete, rename, or configure one.
group: Branches and tags
order: 1
---

## Synopsis

```text
grit branch
grit branch <name> [<start>]
grit branch -d <name>
grit branch -D <name>
grit branch -m [<old>] <new>
grit branch -M <new>
grit branch -u <upstream> [<name>]
grit branch --unset-upstream [<name>]
grit branch [--remote | --all] [-n <N>]
grit branch [--dry-run] …
```

## Description

With no arguments, lists your local branches and marks the current one with `*`. Use `--remote` for remote-tracking branches (`origin/main`, …) and `--all` for both. `-n` / `--limit` caps how many names are returned after sorting by name.

Upstream and ahead/behind counts appear only in `--json` / `--markdown` when an upstream is configured (plain list stays cheap). JSON list entries also include `oid`, `short_oid`, and `is_remote`.

With a name, creates `refs/heads/<name>` at `<start>` or at `HEAD`. It does not switch to the new branch; use [`grit switch -c`](../switch/) to create and switch in one step.

Branch names follow the same rules as `git check-ref-format --branch`. Invalid names fail before anything is written under `.git/refs`.

With `-d`, deletes a branch when its tip is reachable from the current branch's upstream (if configured) or from `HEAD`. With `-D` or `--force-delete`, deletes even when commits would otherwise be lost. You cannot delete the branch you are on, or a branch checked out in another linked worktree.

With `-m` / `--move`, renames a branch (one argument renames the current branch). `-M` is the same as `-m --force`, replacing an existing `refs/heads/<new>` when needed.

With `-u` / `--set-upstream-to`, writes `branch.<name>.remote` and `branch.<name>.merge` from a local or remote-tracking branch. With `--unset-upstream`, removes those keys.

Use `--dry-run` on any mutation to preview the JSON outcome without changing refs, config, or reflogs.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success, including `--dry-run` previews. |
| `1` | Branch not found, not fully merged, checked out elsewhere, invalid start point, upstream error, or other operational failure. |
| `2` | Usage error (missing arguments, incompatible flags). |

On failure with `--json`, stdout includes `error` and a stable `kind` (for example `branch_not_fully_merged`, `branch_not_found`, `branch_already_exists`).

## Options

| Option | Description |
| --- | --- |
| `<name>` | Branch to create, delete, rename, or configure. Omit to list. |
| `<start>` | Commit to start from when creating (`rev-parse` syntax). Defaults to `HEAD`. |
| `-f`, `--force` | On create or rename, replace an existing branch tip (refuses when that branch is checked out in a worktree). |
| `-t`, `--track` | When creating, always configure upstream from `<start>` when possible. |
| `--no-track` | When creating, never configure upstream. |
| `-u`, `--set-upstream-to` | Set upstream for `<name>` or the current branch when `<name>` is omitted. |
| `--unset-upstream` | Clear upstream for `<name>` or the current branch. |
| `-m`, `--move` | Rename: `-m <new>` or `-m <old> <new>`. |
| `-M` | Force rename: `-M <new>` (current branch) or `-M <old> <new>`. |
| `-d`, `--delete` | Delete the branch when it is fully merged. |
| `-D`, `--force-delete` | Delete even when not fully merged. |
| `--remote` | List remote-tracking branches only (list mode). |
| `--all` | List local and remote-tracking branches (list mode). |
| `-n`, `--limit` | Return at most this many branches when listing. |
| `--dry-run` | Preview without writing refs, config, or reflogs. |

`--force` does not delete branches. Use `--force-delete` (or `-D`) instead; passing `--force` with `--delete` is a usage error.

## Examples

```console
$ grit branch
  feature
* main

$ grit branch experiment
Created branch experiment at a1b2c3d

$ grit branch topic main~1
Created branch topic at 9f8e7d6

$ grit branch -d experiment
Deleted branch experiment (was cf18394).

$ grit branch -m topic renamed
Renamed branch topic to renamed

$ grit branch -u origin/main renamed
Set upstream of renamed to origin/main
```

Deleting a branch with unmerged work:

```console
$ grit branch -d spike
error: the branch 'spike' is not fully merged into 'main'
If you are sure you want to delete it, run 'grit branch --force-delete spike'

$ grit branch -D spike
Deleted branch spike (was a7020c2).
```

## JSON output

Schema: [v1](/schemas/branch/v1.json)

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, `delete`, `rename`, `set_upstream`, or `unset_upstream`. |
| `current` | string or null | For `list`, the current branch, or `null` when detached. |
| `branches` | array | For `list`, each branch (see below). |
| `branches[].name` | string | Short name (`main`, `origin/main`, …). |
| `branches[].current` | boolean | Whether this is the checked-out local branch. |
| `branches[].is_remote` | boolean | True for remote-tracking refs. |
| `branches[].oid` | string | Tip object id (full hex). |
| `branches[].short_oid` | string | Abbreviated tip id (seven hex digits when possible). |
| `branches[].upstream` | object or omitted | For local branches with upstream config: `name`, optional `ahead` / `behind`, and `gone` when the upstream ref is missing. |
| `name` | string | For `create` or `delete`, the branch name. |
| `oid` | string | For `create` or `delete`, full tip id. For `rename`, tip before rename when present. |
| `short_oid` | string | Abbreviated tip id (seven hex digits when possible). |
| `upstream` | string or null | For `create`, configured upstream display name when tracking applies; for `set_upstream`, the upstream display name (for example `origin/main`). |
| `old` | string | For `rename`, previous short name. |
| `new` | string | For `rename`, new short name. |
| `replaced` | string or null | For `rename` with `--force`, previous tip of `new` when overwritten. |
| `branch` | string | For `set_upstream` / `unset_upstream`, the branch that was updated. |
| `dry_run` | boolean | Present and `true` when the operation was preview-only. |

Listing branches:

```json
{
  "action": "list",
  "current": "main",
  "branches": [
    {
      "name": "feature",
      "current": false,
      "is_remote": false,
      "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
      "short_oid": "a1b2c3d"
    },
    {
      "name": "main",
      "current": true,
      "is_remote": false,
      "oid": "b2c3d4e5f6789012345678901234567890abcde1",
      "short_oid": "b2c3d4e"
    }
  ]
}
```

Remote-tracking list:

```json
{
  "action": "list",
  "current": "main",
  "branches": [
    {
      "name": "origin/main",
      "current": false,
      "is_remote": true,
      "oid": "b2c3d4e5f6789012345678901234567890abcde1",
      "short_oid": "b2c3d4e"
    }
  ]
}
```

Creating a branch:

```json
{
  "action": "create",
  "name": "experiment",
  "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
  "short_oid": "a1b2c3d"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/branch/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit switch](../switch/), [grit merge](../merge/)
