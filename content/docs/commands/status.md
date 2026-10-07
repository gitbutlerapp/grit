---
title: grit status
summary: Show where you are and what's changed. This is what plain grit runs.
group: Getting started
order: 3
---

## Synopsis

```
grit
grit status
grit st
```

## Description

The status screen is your home base. It shows, from top to bottom:

- **Where you are.** The current branch and how it compares with its target branch: how many commits ahead it is, followed by those commits (up to ten). A branch with no commits says `no commits yet`, and a detached HEAD shows the commit it's on.
- **Staged** changes, which are ready to commit.
- **Changed (not staged)** files, which have been modified since they were last staged.
- **Untracked** files, which `grit` isn't tracking yet.
- **A hint** with the next command to run.

Each change has a label: `new`, `modified`, `deleted`, `renamed`, `copied`, `type changed` or `conflict`.

Paths are shown relative to the directory you run `grit` from.

### The target branch

The target is the branch your work is headed for. `grit` uses the first of these that exists:

1. the branch named in the `target.branch` config setting
2. `origin/master`
3. `origin/main`
4. `master`
5. `main`

To compare against something else, set it for the repository:

```
$ grit config target.branch origin/develop
```

## Options

`grit status` takes no options beyond the [global ones](../global-options/).

## Examples

A branch with work in progress:

```
$ grit
On feature  ·  2 ahead of origin/main

  cf18394  ada  2 hours ago  Say hi
  9a1c2e0  ada  3 hours ago  Add a greeting test

Staged
  +  new           notes.md

Changed (not staged)
  ~  modified      main.rs

Untracked
  ?  untracked     scratch.txt

→ grit add <file> to stage  ·  grit commit "message" to commit
```

Everything committed and pushed:

```
$ grit st
On main  ·  even with origin/main

Nothing to commit — working tree clean.
```

Check from a script whether the working tree is clean:

```
$ grit status --json --filter .clean
true
```

## JSON output

```
{
  "branch": "feature",
  "detached": false,
  "head": "cf18394a62c3f845bd9c44927a5a55e014b2a99d",
  "target": "origin/main",
  "ahead": 2,
  "commits": [
    { "oid": "cf18394a62c3f845bd9c44927a5a55e014b2a99d", "subject": "Say hi" },
    { "oid": "9a1c2e0d4b6f8a1c3e5d7f9b0a2c4e6d8f0a1b3c", "subject": "Add a greeting test" }
  ],
  "staged": [
    { "path": "notes.md", "status": "added" }
  ],
  "unstaged": [
    { "path": "main.rs", "status": "modified" }
  ],
  "untracked": ["scratch.txt"],
  "clean": false
}
```

| Field | Description |
| --- | --- |
| `branch` | The current branch, or `null` when HEAD is detached. |
| `detached` | Whether HEAD is detached. |
| `head` | The full id of the current commit, or `null` before the first commit. |
| `target` | The target branch, or `null` when none was found. |
| `ahead` | How many commits the branch has that the target doesn't. |
| `commits` | The newest of those commits, newest first, up to ten. |
| `staged`, `unstaged` | Changes, each with a `path` and a `status`: `added`, `modified`, `deleted`, `renamed`, `copied`, `type_changed` or `unmerged`. |
| `untracked` | Paths of untracked files. |
| `clean` | `true` when there is nothing to commit and nothing untracked. |

## See also

[grit shortlog](../shortlog/), [grit diff](../diff/), [grit add](../add/), [grit commit](../commit/)
