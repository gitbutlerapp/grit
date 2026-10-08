---
title: grit switch
summary: Switch to another branch, or create one and switch to it.
group: Branches and tags
order: 2
---

## Synopsis

```text
grit switch <branch>
grit switch -c <branch>
```

`grit checkout` and `grit co` do the same thing.

## Description

Switches to a local branch and updates the files in your working tree to match it.

`grit switch` won't run while you have uncommitted changes, staged or not, so that nothing gets carried over to the wrong branch by accident. Commit first. Untracked files stay where they are, unless the other branch has a file at the same path; then `grit` stops rather than overwrite it.

With `-c`, creates the branch at the current commit and switches to it.

## Options

| Option | Description |
| --- | --- |
| `<branch>` | The branch to switch to, or to create with `-c`. |
| `-c`, `--create` | Create the branch first, then switch to it. Fails if the branch already exists. |

## Examples

```console
$ grit switch -c feature
Created and switched to branch feature

$ grit switch main
Switched to branch main
```

With uncommitted changes:

```console
$ grit switch main
error: you have uncommitted changes — commit them before switching
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string | The branch you switched to. |
| `created` | boolean | `true` when `-c` created the branch; otherwise `false`. |

Example:

```json
{
  "branch": "feature",
  "created": true
}
```

## See also

[grit branch](../branch/), [grit merge](../merge/), [grit status](../status/)
