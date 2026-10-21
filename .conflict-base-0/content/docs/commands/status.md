---
title: grit status
summary: Show where you are and what's changed. This is what plain grit runs.
group: Getting started
order: 3
---

## Synopsis

```text
grit
grit status
grit st
```

## Description

The status screen is your home base. It shows, from top to bottom:

- **Where you are.** The current branch and how it compares with its target branch: ahead and behind counts, followed by ahead commits (up to ten). A branch with no commits says `no commits yet`, and a detached HEAD shows the commit it's on.
- **Staged** changes, which are ready to commit.
- **Changed (not staged)** files, which have been modified since they were last staged.
- **Untracked** files, which `grit` isn't tracking yet.
- **A hint** with the next command to run.

Each change has a label: `new`, `modified`, `deleted`, `renamed`, `copied`, `type changed` or `conflict`.

Paths are shown relative to the directory you run `grit` from. Valid UTF-8 file names print as-is in human and `--markdown` output (unlike Git’s default `core.quotepath=true`, which octal-escapes non-ASCII bytes). Names that contain C0 control characters (including ESC and BEL) are still C-quoted so a malicious repository cannot inject terminal escape sequences. Set `core.quotepath=true` in config if you want Git-style octal escaping for all non-ASCII bytes. `--json` path fields are always unquoted UTF-8 strings (JSON escapes controls as `\uXXXX`).

### The target branch

The target is the branch your work is compared against in the header line. When the current branch has upstream tracking configured (`branch.<name>.remote` and `branch.<name>.merge`, including after `grit switch origin/<topic>` creates a local branch), `grit` compares against that upstream first.

Otherwise it uses the first of these that exists:

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

| Flag | Description |
| ---- | ----------- |
| `--untracked <MODE>` | How untracked paths are listed: `normal` (default) shows untracked files and collapses wholly untracked directories to a single directory entry; `all` lists every untracked file; `no` omits untracked paths. When the flag is omitted, `status.showUntrackedFiles` from config is used (Git-compatible; defaults to `normal`). |

See also the [global options](../global-options/).

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

Behind the target after a fetch:

```console
$ grit status
On main  ·  1 behind origin/main

Nothing to commit — working tree clean.

→ grit pull to update from origin/main
```

Diverged from the target:

```console
$ grit status
On main  ·  1 ahead, 1 behind origin/main

  abc1234  you  1 hour ago  Local work

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

List every untracked file (for staging scripts and agents):

```console
$ grit status --untracked all --json --filter .untracked
[
  "docs/a.txt",
  "docs/b.txt",
  "plain.txt",
  "src/main.rs"
]
```

With the default `normal` mode, the same tree collapses directories:

```console
$ grit status --json --filter .untracked
[
  "docs/",
  "plain.txt",
  "src/"
]
```

## JSON output

Schema: [v1](/schemas/status/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `branch` | string or null | Current branch, or `null` when HEAD is detached. |
| `detached` | boolean | Whether HEAD is detached. |
| `head` | string or null | Full id of the current commit, or `null` before the first commit. |
| `target` | string or null | Target branch, or `null` when none was found. |
| `ahead` | number | Commits on the branch that the target does not have. |
| `behind` | number | Commits on the target that the branch does not have. |
| `commits` | array | Newest of those commits, up to ten, each with `oid`, `subject`, and `author` (`name`, `email`, `date` — same shape as [`grit show`](../show/)). |
| `staged` | array | Staged changes with `path` and `status`. |
| `unstaged` | array | Unstaged changes with `path` and `status`. |
| `untracked` | array | Untracked paths (raw UTF-8; JSON escapes control characters). With default `--untracked normal`, wholly untracked directories appear once (often with a trailing `/`); with `--untracked all`, every file path is listed. |
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
  "behind": 0,
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

Schema: [v1](/schemas/status/v1.json)
Pass [`--markdown`](../global-options/) for headed sections that mirror the status screen:

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

[grit shortlog](../shortlog/), [grit diff](../diff/), [grit add](../add/), [grit commit](../commit/)
