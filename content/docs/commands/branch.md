---
title: grit branch
summary: List branches, or create or delete one.
group: Branches and tags
order: 1
---

## Synopsis

```
grit branch
grit branch <name>
grit branch -d <name>
grit branch -D <name>
```

## Description

With no arguments, lists your local branches and marks the current one with `*`.

With a name, creates a branch at the current commit. It doesn't switch to the new branch; use [`grit switch -c`](../switch/) to create a branch and switch to it in one step.

With `-d`, deletes a branch. `grit` refuses if the branch has commits that aren't in the current branch, so you can't lose work by accident; `-D` deletes it anyway. You can't delete the branch you're on.

## Options

| Option | Description |
| --- | --- |
| `<name>` | The branch to create or delete. Omit to list branches. |
| `-d`, `--delete` | Delete the branch. It must be fully merged into the current branch. |
| `-D`, `--force` | Delete the branch even if it isn't merged. |

## Examples

```
$ grit branch
  feature
* main

$ grit branch experiment
Created branch experiment

$ grit branch -d experiment
Deleted branch experiment (was cf18394).
```

Deleting a branch with unmerged work:

```
$ grit branch -d spike
error: the branch 'spike' is not fully merged.
If you are sure you want to delete it, run 'grit branch -D spike'

$ grit branch -D spike
Deleted branch spike (was a7020c2).
```

## JSON output

Pass `--json` for stable, scripting-friendly output. The object's `action` field says what happened:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `action` | string | `list`, `create`, or `delete`. |
| `current` | string or null | For `list`, the current branch, or `null` when detached. |
| `branches` | array | For `list`, each branch with `name` and `current`. |
| `name` | string | For `create` or `delete`, the branch name. |
| `oid` | string | For `delete`, full id of the tip before deletion. |
| `short_oid` | string | For `delete`, abbreviated tip id. |

Listing branches:

```json
{
  "action": "list",
  "current": "main",
  "branches": [
    { "name": "feature", "current": false },
    { "name": "main", "current": true }
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

## See also

[grit switch](../switch/), [grit merge](../merge/)
