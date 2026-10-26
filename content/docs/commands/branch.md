---
title: grit branch
summary: List branches, or create, delete, rename, or configure one.
group: Branches and tags
order: 1
---

## Synopsis

```text
grit branch [--list <glob>]... [-r | -a] [--merged [REV]] [--no-merged [REV]]
            [--contains [REV]] [--sort name|version|date] [--reverse] [-n <N>]
            [--show-current]
grit branch <name> [<start>]
grit branch -d <name>
grit branch -D <name>
grit branch -m [<old>] <new>
grit branch -M <new>
grit branch -u <upstream> [<name>]
grit branch --unset-upstream [<name>]
grit branch [--dry-run] …
```

## Description

With no arguments, lists your local branches and marks the current one with `*`. When a branch has a configured upstream, the line includes a short marker such as `[origin/main: ahead 2, behind 1]` or `[gone]` when the tracking ref is missing.

Use `-r` / `--remotes` for remote-tracking branches (`origin/main`, …) and `-a` / `--all` for both (remote names use the `remotes/origin/main` form, like `git branch -a`). `--list`, `--merged`, `--no-merged`, `--contains`, `--sort`, `--reverse`, and `-n` / `--limit` follow Git’s branch listing filters (`--max-count` is an alias). `--show-current` prints only the current branch name.

With `--json` or `--markdown`, list output includes `full_ref`, commit `subject` and `date`, upstream details, and linked `worktree` paths when present. Plain human list stays names plus upstream markers.

With a name, creates `refs/heads/<name>` at `<start>` or at `HEAD`. It does not switch to the new branch; use [`grit switch -c`](../switch/) to create and switch in one step.

Branch names follow the same rules as `git check-ref-format --branch`. Invalid names fail before anything is written under `.git/refs`.

With `-d`, deletes a branch when its tip is reachable from the current branch's upstream (if configured) or from `HEAD`. With `-D` or `--force-delete`, deletes even when commits would otherwise be lost. You cannot delete the branch you are on, or a branch checked out in another linked worktree.

With `-m` / `--move`, renames a branch (one argument renames the current branch). `-M` is the same as `-m --force`, replacing an existing `refs/heads/<new>` when needed.

With `-u` / `--set-upstream-to`, writes `branch.<name>.remote` and `branch.<name>.merge` from a local or remote-tracking branch. With `--unset-upstream`, removes those keys.

Use `--dry-run` on any mutation to preview the JSON outcome without changing refs, config, or reflogs.

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
| `--list <glob>` | Include only branches whose short name matches `GLOB` (repeatable). |
| `-r`, `--remotes` | List remote-tracking branches (list mode). |
| `-a`, `--all` | List local and remote-tracking branches (list mode). |
| `--merged [REV]` | Branches merged into `REV` (default `HEAD`). |
| `--no-merged [REV]` | Branches not merged into `REV` (default `HEAD`). |
| `--contains [REV]` | Branches whose tip contains `REV` (default `HEAD`). |
| `--sort name\|version\|date` | Sort key (default name). |
| `--reverse` | Reverse sort order. |
| `-n`, `--limit <N>` | Limit the number of branches listed (`--max-count` alias). |
| `--show-current` | Print only the current branch name (nothing when detached). |
| `--dry-run` | Preview without writing refs, config, or reflogs. |

`--force` does not delete branches. Use `--force-delete` (or `-D`) instead; passing `--force` with `--delete` is a usage error.

## Examples

```console
$ grit branch
  feature
* main [origin/main: ahead 1, behind 0]

$ grit branch --list 'feat*' --sort date -n 5
  feature-old
* feature

$ grit branch --show-current
main

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

List results include `truncated: true` when `-n` / `--limit` caps the branch list and more branches matched. With `--contains` and name ordering (including `--reverse`), the limit bounds matching rows: grit checks tips in name order and stops after the extra match needed to detect truncation. Earlier nonmatching branches do not consume the limit.

Schema: [v1](/schemas/branch/v1.json)

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, `delete`, `rename`, `set_upstream`, or `unset_upstream`. |
| `current` | string or null | For `list`, the current branch, or `null` when detached. |
| `branches` | array | For `list`, each branch (see below). |
| `truncated` | boolean | For `list`: `true` when more branches matched than returned (`-n` / `--limit`). |
| `branches[].name` | string | Short name (`main`, `origin/main`, `remotes/origin/main` with `-a`, …). |
| `branches[].current` | boolean | Whether this is the checked-out local branch. |
| `branches[].is_remote` | boolean | True for remote-tracking refs. |
| `branches[].full_ref` | string | Full ref (`refs/heads/…`, `refs/remotes/…`). |
| `branches[].oid` | string | Tip object id (full hex). |
| `branches[].short_oid` | string | Abbreviated tip id (seven hex digits when possible). |
| `branches[].subject` | string or null | Tip commit subject (`--json` / `--markdown` list). |
| `branches[].date` | string or null | Tip committer time (RFC 3339). |
| `branches[].upstream` | object or omitted | `ref`, optional `remote`, `gone`, `ahead`, `behind`. |
| `branches[].worktree` | string or null | Linked worktree path when checked out elsewhere. |
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
      "full_ref": "refs/heads/feature",
      "oid": "a1b2c3d4e5f6789012345678901234567890abcd",
      "short_oid": "a1b2c3d",
      "subject": "Add feature",
      "date": "2023-11-14T22:13:20Z",
      "upstream": {
        "ref": "origin/feature",
        "remote": "origin",
        "gone": false,
        "ahead": 0,
        "behind": 2
      }
    },
    {
      "name": "main",
      "current": true,
      "is_remote": false,
      "full_ref": "refs/heads/main",
      "oid": "b2c3d4e5f6789012345678901234567890abcde1",
      "short_oid": "b2c3d4e",
      "subject": "Initial commit",
      "date": "2023-11-14T22:13:20Z"
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

Markdown lists branches in a table with upstream and worktree columns when present. Table cells escape `|` and internal newlines so commit subjects and paths stay on one row. See the JSON output section for the stable field contract.



```text
## Branches
| Branch | Current | OID | Subject | Upstream | Worktree |
| --- | --- | --- | --- | --- | --- |
| `feature` |  | `062ca6e` | first |  |  |
| `main` | yes | `83352a6` | second |  | [REPO] |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success, including `--dry-run` previews. |
| `1` | Branch not found, not fully merged, checked out elsewhere, or other operational failure. |
| `2` | Usage error (missing arguments, incompatible flags). |

On failure with `--json`, stdout includes `error` and a stable `kind`. See [Exit codes](../exit-codes/).


## See also

[grit switch](../switch/), [grit merge](../merge/)