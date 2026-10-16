# grit status

> Show where you are and what's changed. This is what plain grit runs.

## Synopsis

```text
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

```console
$ grit config target.branch origin/develop
```

## Options

`grit status` takes no options beyond the [global ones](https://grit-scm.com/docs/global-options/index.md).

## Examples

A branch with work in progress:

```console
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

```console
$ grit st
On main  ·  even with origin/main

Nothing to commit — working tree clean.
```

Merge in progress with a conflict:

```console
$ grit status
On main  ·  merging — resolve conflicts

Staged
  !  conflict      f

→ resolve conflicts, then grit commit "message"
```

Check from a script whether the working tree is clean:

```console
$ grit status --json --filter .clean
true
```

## JSON output

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string or null | Current branch, or `null` when HEAD is detached. |
| `detached` | boolean | Whether HEAD is detached. |
| `head` | string or null | Full id of the current commit, or `null` before the first commit. |
| `target` | string or null | Target branch, or `null` when none was found. |
| `ahead` | number | Commits on the branch that the target does not have. |
| `commits` | array | Newest of those commits, up to ten, each with `oid` and `subject`. |
| `staged` | array | Staged changes with `path` and `status`. |
| `unstaged` | array | Unstaged changes with `path` and `status`. |
| `untracked` | array | Paths of untracked files. |
| `clean` | boolean | `true` when there is nothing to commit and nothing untracked. |
| `merging` | boolean | `true` when a merge is in progress (`MERGE_HEAD` exists). |
| `in_progress` | array | Stable operation ids while work is paused (for example `merge`, `rebase`). Omitted when empty. |
| `conflicts` | array | Paths with unmerged index stages. Omitted when empty. |

Example:

```json
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

## Markdown output

Pass [`--markdown`](https://grit-scm.com/docs/global-options/index.md) for headed sections that mirror the status screen:

```text
On **`feature`** · **2** ahead of `origin/main`

- `cf18394` Say hi (ada, 1 hour ago)

Staged
- **new** `notes.md`

Changed (not staged)
- **modified** `main.rs`

Untracked
- `scratch.txt`
```

## See also

[grit shortlog](https://grit-scm.com/docs/shortlog/index.md), [grit diff](https://grit-scm.com/docs/diff/index.md), [grit add](https://grit-scm.com/docs/add/index.md), [grit commit](https://grit-scm.com/docs/commit/index.md)
