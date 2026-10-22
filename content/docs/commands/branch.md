---
title: grit branch
summary: List branches, or create or delete one.
group: Branches and tags
order: 1
---

## Synopsis

```text
grit branch [--list <glob>]... [-r | -a] [--merged [REV]] [--no-merged [REV]]
            [--contains [REV]] [--sort name|version|date] [--reverse] [-n <N>]
            [--show-current]
grit branch <name>
grit branch -d <name>
grit branch -D <name>
```

## Description

With no arguments, lists your local branches and marks the current one with `*`. When a branch has a configured upstream, the line includes a short marker such as `[origin/main: ahead 2, behind 1]` or `[gone]` when the tracking ref is missing.

With a name, creates a branch at the current commit. It doesn't switch to the new branch; use [`grit switch -c`](../switch/) to create a branch and switch to it in one step.

Branch names follow the same rules as `git check-ref-format --branch` (no spaces, `..`, `~`, `.lock`, reserved names like `HEAD`, and other characters Git rejects). Invalid names fail before anything is written under `.git/refs`.

With `-d`, deletes a branch. `grit` refuses if the branch has commits that aren't in the current branch, so you can't lose work by accident; `-D` deletes it anyway. You can't delete the branch you're on, or a branch that is checked out in any linked worktree (including worktrees added with `git worktree add`).

## Options

| Option | Description |
| --- | --- |
| `<name>` | The branch to create or delete. Omit to list branches. |
| `--list <glob>` | Include only branches whose short name matches `GLOB` (repeatable). |
| `-r`, `--remotes` | List remote-tracking branches. |
| `-a`, `--all` | List local and remote-tracking branches. |
| `--merged [REV]` | Branches merged into `REV` (default `HEAD`). |
| `--no-merged [REV]` | Branches not merged into `REV` (default `HEAD`). |
| `--contains [REV]` | Branches whose tip contains `REV` (default `HEAD`). |
| `--sort name\|version\|date` | Sort key (default name). |
| `--reverse` | Reverse sort order. |
| `-n`, `--max-count <N>` | Limit the number of branches listed. |
| `--show-current` | Print only the current branch name (nothing when detached). |
| `-d`, `--delete` | Delete the branch. It must be fully merged into the current branch. |
| `-D`, `--force` | Delete the branch even if it isn't merged. |

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
Created branch experiment

$ grit branch -d experiment
Deleted branch experiment (was cf18394).
```

Deleting a branch with unmerged work:

```console
$ grit branch -d spike
error: the branch 'spike' is not fully merged.
If you are sure you want to delete it, run 'grit branch -D spike'

$ grit branch -D spike
Deleted branch spike (was a7020c2).
```

## JSON output

Schema: [v1](/schemas/branch/v1.json)

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, or `delete`. |
| `current` | string or null | For `list`, the current branch, or `null` when detached. |
| `branches` | array | For `list`, each branch with `name`, `current`, `full_ref`, `oid`, optional `remote`, `subject`, `date`, `upstream`, and `worktree`. |
| `name` | string | For `create` or `delete`, the branch name. |
| `oid` | string | For `delete`, full id of the tip before deletion. |
| `short_oid` | string | For `delete`, abbreviated tip id. |

Listing branches:

```json
{
  "action": "list",
  "current": "main",
  "branches": [
    {
      "name": "feature",
      "current": false,
      "full_ref": "refs/heads/feature",
      "oid": "abc123…",
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
      "full_ref": "refs/heads/main",
      "oid": "def456…",
      "subject": "Initial commit",
      "date": "2023-11-14T22:13:20Z"
    }
  ]
}
```

Creating a branch:

```json
{
  "action": "create",
  "name": "experiment"
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/branch/v1.json)

Markdown lists branches in a table with upstream and worktree columns when present. See the JSON output section for the stable field contract.

## See also

[grit switch](../switch/), [grit merge](../merge/)
