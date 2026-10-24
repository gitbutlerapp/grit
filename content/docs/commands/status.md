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

During a paused merge or grit rebase, human output also lists **Conflicts** grouped by kind (`both modified`, `both added`, …). In `--json`, each conflict is `{ "path", "kind" }` and `operation.next_steps` lists the ordered `grit` commands while an operation is paused (for example `grit diff` then `grit add .` with conflicts, or `grit merge --continue` / `grit rebase --continue` once conflicts are resolved). Use `operation.alternatives` for cancel paths such as `grit merge --abort` or `grit rebase --abort` (not part of the ordered sequence). Grit rebase sessions are detected via `.git/rebase-merge/grit-plan`.

A rebase that `git` started (`.git/rebase-merge/` or `.git/rebase-apply/` without grit's plan) is recognised too. The header names the branch being rebased (`Rebasing topic onto 1a2b3c4`), JSON `branch` reports that branch, and `operation` is `rebase-interactive` (Git's default merge backend) or `rebase` (`git rebase --apply`). Its `next_steps` end with `git rebase --continue`, because only git can resume its own todo list. `grit rebase --abort` is listed in `alternatives` and cancels it. Conflict kinds match Git porcelain v2 unmerged XY codes: `both_modified` (UU), `both_added` (AA), `both_deleted` (DD), `added_by_us` (AU), `added_by_them` (UA), `deleted_by_us` (DU), `deleted_by_them` (UD).

Paths are shown relative to the directory you run `grit` from. Valid UTF-8 file names print as-is in human and `--markdown` output (unlike Git’s default `core.quotepath=true`, which octal-escapes non-ASCII bytes). Names that contain C0 control characters (including ESC and BEL) are still C-quoted so a malicious repository cannot inject terminal escape sequences. Set `core.quotepath=true` in config if you want Git-style octal escaping for all non-ASCII bytes. `--json` path fields are always unquoted UTF-8 strings (JSON escapes controls as `\uXXXX`).

### The target branch

The target is the branch your work is compared against in the header line (`target`, `ahead`, and `behind` in JSON). It uses the first of these that exists:

1. the branch named in the `target.branch` config setting
2. `origin/master`
3. `origin/main`
4. `master`
5. `main`

To compare against something else, set it for the repository:

```console
$ grit config target.branch origin/develop
```

When the current branch has a configured upstream (`branch.<name>.remote` and `branch.<name>.merge`) that differs from the target, human output adds a second line (`↑n ↓m origin/topic`) and JSON includes an `upstream` object with the same counts relative to that tracking ref. When upstream and target resolve to the same ref, `upstream` is `null`.

## Options

| Flag | Description |
| ---- | ----------- |
| `--untracked <MODE>` | How untracked paths are listed: `normal` (default) shows untracked files and collapses wholly untracked directories to a single directory entry; `all` lists every untracked file; `no` omits untracked paths. When the flag is omitted, `status.showUntrackedFiles` from config is used (Git-compatible; defaults to `normal`). |
| `--no-ahead-behind` | Skip ahead/behind rev-list walks; JSON sets `ahead`, `behind`, and upstream counts to `null`. Also skipped when `status.aheadBehind=false` in config. |
| `PATH`… | Limit the report to paths matching these pathspecs (after `--`). Uses the same pathspec rules as [`grit add`](../add/). |
| `--pathspec-from-file <FILE\|->` | Read pathspecs from a file (`-` for stdin), in addition to any `PATH` arguments. |
| `--pathspec-file-nul` | `--pathspec-from-file` entries are NUL-separated (requires `--pathspec-from-file`). |
| `--ignored` | List ignored paths (traditional mode; same `!!` set as `git status --ignored --porcelain`). |
| `--short` | Compact one-line-per-path output for humans only (two-letter staged/unstaged codes; JSON unchanged). |

Rename detection is on by default (Git’s `status.renames` / `diff.renames`; disable with `status.renames=false`). Human output shows renames as `old → new`; `--json` adds `old_path` on rename and copy entries.

### How changes are detected

Like Git, `grit status` compares each tracked file's size and timestamps with what the index recorded, and reads a file only when they differ. A clean tree costs one file-system lookup per tracked file, whatever the file sizes or `core.filemode`. Files written in the same second as the index (right after `git clone`, a commit or a checkout) cannot be trusted by timestamp alone: status reads them once and saves the refreshed index, so later runs skip them. Like `git status`, it may therefore rewrite `.git/index`; only the cached file information changes, never what is staged.

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

→ grit add <file> to stage  ·  grit merge --continue to finish
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

Conflicts
  both modified (1)
    f

→ grit diff
→ grit add .
```

A rebase that `git` started, stopped on a conflict:

```console
$ grit status
Rebasing topic onto 3f1c2a9

Staged
  !  conflict      f.txt

Conflicts
  both modified (1)
    f.txt

→ grit diff  ·  grit add .  ·  git rebase --continue
or grit rebase --abort
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
| `branch` | string or null | Current branch, or `null` when HEAD is detached. While a rebase is in progress (grit's or git's), the branch being rebased; `null` when the rebase started on a detached HEAD. |
| `detached` | boolean | Whether HEAD is detached. |
| `head` | string or null | Full id of the current commit, or `null` before the first commit. |
| `target` | string or null | Target branch, or `null` when none was found. |
| `ahead` | number or null | Commits on the branch that the target does not have; `null` when ahead/behind counts were skipped. |
| `behind` | number or null | Commits on the target that the branch does not have; `null` when counts were skipped. |
| `upstream` | object or null | When upstream tracking differs from `target`: `{ "ref", "ahead", "behind" }` (counts may be `null` when skipped). Always present; `null` when there is no separate upstream. |
| `commits` | array | Newest of those commits, up to ten, each using the shared [`CommitJson`](../README.md#commit-json-object-commitjson) shape (including `change_id` and `change_id_stored`). |
| `staged` | array | Staged changes with `path`, `status`, and optional `old_path` (renames/copies). |
| `unstaged` | array | Unstaged changes with `path`, `status`, and optional `old_path` (renames/copies). |
| `untracked` | array | Untracked paths (raw UTF-8; JSON escapes control characters). With default `--untracked normal`, wholly untracked directories appear once (often with a trailing `/`); with `--untracked all`, every file path is listed. |
| `clean` | boolean | `true` when there is nothing to commit and nothing untracked. |
| `merging` | boolean | **Deprecated.** Prefer `operation`. `true` when a merge is in progress (`MERGE_HEAD` exists). |
| `in_progress` | array | **Deprecated.** Prefer `operation`. Stable operation ids while work is paused (for example `merge`, `rebase`). Omitted when empty. |
| `conflicts` | array | Unmerged paths. Each item is `{ "path": "<path>", "kind": "<kind>" }`. Omitted when empty. |
| `operation` | object or null | Primary paused operation: `{ "kind", "next_steps", "alternatives" }`. `next_steps` is the ordered sequence; `alternatives` lists mutually exclusive options (for example `grit merge --abort`). Steps are `grit` commands, except `git rebase --continue` for a rebase that git started. Omitted when no operation is in progress. |
| `ignored` | array | Ignored paths when `--ignored` was passed; omitted otherwise. |
| `stash_count` | number | Entries on the stash reflog (`0` when empty). |

Example:

```json
{
  "branch": "main",
  "detached": false,
  "head": "83352a61f6715ec3f0a297c6d6150725154fb833",
  "target": "main",
  "ahead": 0,
  "behind": 0,
  "upstream": null,
  "commits": [],
  "staged": [
    {
      "path": "staged.txt",
      "status": "added"
    }
  ],
  "unstaged": [
    {
      "path": "tracked.txt",
      "status": "modified"
    }
  ],
  "untracked": [
    "untracked.txt"
  ],
  "clean": false,
  "merging": false,
  "stash_count": 0
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


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

[grit shortlog](../shortlog/), [grit diff](../diff/), [grit add](../add/), [grit commit](../commit/)