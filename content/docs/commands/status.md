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

Each change has a label: `new`, `modified`, `deleted`, `renamed`, `copied`, `type changed` or `conflict`. `type changed` (`type_changed` in `--json`, Git's `T`) means the path switched between a regular file, a symlink and a submodule; a permission-only change such as `chmod +x` is `modified`.

**Submodules:** grit records gitlinks in the index but does not fetch nested repositories ([`grit clone`](../clone/) leaves an empty directory and prints a note). An **uninitialized** submodule directory (no embedded `.git`) is ignored the same way Git ignores it: stray files you create inside that placeholder are not listed as modified or untracked, and `grit commit` agrees with a clean `git status`. A **checked-out** submodule with its own `.git` directory is compared by its HEAD commit and inner worktree dirtiness.

A tracked file that grit cannot open to compare its content (another program holds it open without sharing, or it has no read permission) is listed as `modified`, like Git does, and the rest of the status is reported as usual. When a **directory** on the path cannot be opened (for example `chmod 000` on a parent of a tracked file or an untracked tree), grit prints a `warning:` on stderr naming the path, skips that subtree, and continues — the rest of status is unchanged and exit status stays `0`. With `--json`, those warnings appear in the `warnings` array alongside the normal status fields. Commands that need the file's bytes, such as `grit add` of that path or a patch from `grit diff`, still fail with [`worktree_path_io`](../exit-codes/), and `grit switch` refuses a branch that would change it.

**Index refresh:** after stat data in the work tree no longer matches `.git/index`, grit re-verifies content and normally writes the refreshed stat cache back to the index (like `git status`). If another command updates the index after status loaded it (for example a concurrent commit), grit skips the write and keeps the on-disk index that command wrote. Use [`--no-optional-locks`](../global-options/) or `GIT_OPTIONAL_LOCKS=0` when a background status must not touch the index file; grit still refreshes stat data in memory so the report matches a full run. Library callers set [`StatusOptions::write_index`](rustdoc:grit_lib::porcelain::status::StatusOptions::write_index) to `false` for the same read-only behavior without changing the process environment.

During a paused merge or grit rebase, human output also lists **Conflicts** grouped by kind (`both modified`, `both added`, …). In `--json`, each conflict is `{ "path", "kind" }` and `operation.next_steps` lists the ordered `grit` commands while an operation is paused (for example `grit diff` then `grit add .` with conflicts, or `grit merge --continue` / `grit rebase --continue` once conflicts are resolved). Use `operation.alternatives` for cancel paths such as `grit merge --abort` or `grit rebase --abort` (not part of the ordered sequence). Grit rebase sessions are detected via `.git/rebase-merge/grit-plan`.

A rebase that `git` started (`.git/rebase-merge/` or `.git/rebase-apply/` without grit's plan) is recognised too. The header names the branch being rebased (`Rebasing topic onto 1a2b3c4`), JSON `branch` reports that branch, and `operation` is `rebase-interactive` (Git's default merge backend) or `rebase` (`git rebase --apply`). Its `next_steps` end with `git rebase --continue`, because only git can resume its own todo list. `grit rebase --abort` is listed in `alternatives` and cancels it.

### Conflict kinds

Every grit command that reports conflicts (`status`, `merge`, `pick`, `revert`, `rebase` and `pull`) names each conflicted path with the same `kind`. The kind says which sides still have the path: "us" is the side you were on when the operation started (`HEAD`), "them" is the side being brought in (the merged branch, the picked or reverted commit, or the commit a rebase is replaying). So a conflict reads the same in an operation's output and in `grit status` afterwards, and matches the unmerged XY code of `git status --porcelain=v2`:

| `kind` | Human label | Git XY | Meaning |
| ------ | ----------- | ------ | ------- |
| `both_modified` | both modified | `UU` | Both sides changed the file. |
| `both_added` | both added | `AA` | Both sides added the path with different content. |
| `both_deleted` | both deleted | `DD` | Both sides removed the path. |
| `added_by_us` | added by us | `AU` | Only our side has the path. |
| `added_by_them` | added by them | `UA` | Only their side has the path. |
| `deleted_by_us` | deleted by us | `DU` | We deleted the path and they changed it. |
| `deleted_by_them` | deleted by them | `UD` | We changed the path and they deleted it. |
| `rename_rename` | — | — | Both sides renamed one file to different names (`merge`, `pick`, `revert`, `rebase` and `pull` only; `status` shows the three paths as `both_deleted`, `added_by_us` and `added_by_them`). |
| `directory_rename_moved` | moved with its directory | — | A file one side added to a directory the other side renamed was moved into the new directory; confirm the location (operations only; `status` shows the path as `added_by_us` or `added_by_them`). See [directory renames](../merge/#directory-renames). |
| `directory_rename_split` | directory split up | — | A directory was spread over several directories with no clear destination, so files added to it stayed put. The path is the directory; nothing is unmerged (operations only). |
| `directory_rename_blocked` | directory move blocked | — | Files were not moved into a renamed directory because their destination is taken. The path is the destination; nothing is unmerged (operations only). |

### Interrupted rebases and history edits

A `grit rebase` or `grit history` edit that is killed after moving branches but before the files are updated leaves a recovery journal in `.git/grit/rewrite-apply/`. With optional locks enabled (the default), `grit status` finishes that work before it reports anything, and says what it did in one line above the usual screen. With `--no-optional-locks` / `GIT_OPTIONAL_LOCKS=0`, recovery is deferred and the outcome stays **Pending** until a status run that may write the index:

- **Finished the checkout.** The branches had already moved, so status puts back the files the interrupted checkout was writing and checks out the rewritten commit again. The rebase stands. Unrelated local edits are kept.
- **Rolled back.** When that checkout cannot finish (or for an interrupted `history fixup` or `absorb`), branches, index and files go back to where they were before the command. Status still succeeds.
- **Pending.** Another process holds `.git/index.lock`, so nothing is touched yet. When grit wrote a pid sidecar for that lock (default for grit-owned locks) and the recorded process has exited, the next status removes the stale lock and recovers. When no sidecar exists (for example `core.lockfilePid=false` or a foreign Git lock), the message notes the lock may be stale; run status again after the holder exits, or remove the lock only when nothing else is using the repository.

`--json` reports the outcome in `interrupted_rewrite`. The line and the field appear only on the run that finished or rolled back the work, and on every run while it is pending.

```console
$ grit status
Finished the checkout of an interrupted rebase or history edit; HEAD stays on the rewritten commits.

On feat  ·  1 ahead of master

  4f2a9c1  you  just now  feat

Nothing to commit — working tree clean.
```

Paths are shown relative to the directory you run `grit` from. Valid UTF-8 file names print as-is in human and `--markdown` output (unlike Git’s default `core.quotepath=true`, which octal-escapes non-ASCII bytes). Names that contain C0 control characters (including ESC and BEL) are still C-quoted so a malicious repository cannot inject terminal escape sequences. Set `core.quotepath=true` in config if you want Git-style octal escaping for all non-ASCII bytes. `--json` path fields are always unquoted UTF-8 strings (JSON escapes controls as `\uXXXX`).

### GitButler workspaces

[GitButler](https://gitbutler.com) applies several branches at once by checking out `gitbutler/workspace`, topped by a merge of the applied branches that it maintains itself (its message says `GitButler Workspace Commit`). That commit is GitButler's bookkeeping, so on such a workspace `grit status` lists the applied branches instead of the commits ahead, and leaves the workspace commit out of `commits` and `ahead`. The next-step hint suggests `but commit`, because [`grit commit`](../commit/) refuses to commit there.

```console
$ grit status
On GitButler workspace  ·  2 branches applied  ·  3 behind origin/main

  login-form   4f2a9c1  qzvtyxkl  Ada Lovelace  2 hours ago  Validate the login form
  api-retries  9b3e0d2  mrnwkpst  Ada Lovelace  1 day ago    Retry failed API calls

Nothing to commit — working tree clean.
```

`--json` describes the workspace in `gitbutler_workspace`.

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

On Unix, explicit path operands must be valid UTF-8. An operand containing invalid UTF-8 bytes is rejected before the command runs, with exit status **2** and no changes to the repository. Human output reports the usage error on stderr; `--json` writes an error object with `kind: "usage"` on stdout, and `--markdown` writes `**Error:**` on stdout. The argument is never replaced with a similar Unicode filename. Without a path filter, status still reports non-UTF-8 filenames using reversible `grit+pathhex:` encoding in JSON path fields.

## Options

| Flag | Description |
| ---- | ----------- |
| `--untracked <MODE>` | How untracked paths are listed: `normal` (default) shows untracked files and collapses wholly untracked directories to a single directory entry; `all` lists every untracked file; `no` omits untracked paths. When the flag is omitted, `status.showUntrackedFiles` from config is used (Git-compatible; defaults to `normal`). |
| `--no-ahead-behind` | Skip ahead/behind rev-list walks; JSON sets `ahead`, `behind`, and upstream counts to `null`. Also skipped when `status.aheadBehind=false` in config. |
| `PATH`… | Limit the report to paths matching these pathspecs (after `--`). Uses the same pathspec rules as [`grit add`](../add/). |
| `--pathspec-from-file <FILE\|->` | Read pathspecs from a file (`-` for stdin), in addition to any `PATH` arguments. |
| `--pathspec-file-nul` | `--pathspec-from-file` entries are NUL-separated (requires `--pathspec-from-file`). |
| `--ignored` | List ignored paths (traditional mode; same `!!` set as `git status --ignored --porcelain`). |
| `-s`, `--short` | Compact one-line-per-path output for humans only (two-letter staged/unstaged codes; JSON unchanged). |

From a subdirectory, short status uses cwd-relative paths; untracked directories keep a trailing slash (for example `?? ./` inside an untracked folder, or `?? ../other/` for a sibling directory).

Rename detection is on by default (Git’s `status.renames` / `diff.renames`; disable with `status.renames=false`). Human output shows renames as `old → new`; `--json` adds `old_path` on rename and copy entries.

### How changes are detected

Like Git, `grit status` compares each tracked file's size and timestamps with what the index recorded, and reads a file only when they differ. A clean tree costs one file-system lookup per tracked file, whatever the file sizes or `core.filemode`. Files written in the same second as the index (right after `git clone`, a commit or a checkout) cannot be trusted by timestamp alone: status reads them once and saves the refreshed index, so later runs skip them. Like `git status`, it may therefore rewrite `.git/index`; only the cached file information changes, never what is staged.

A changed size alone counts as a change only when nothing converts the file on the way into the repository. When a line-ending setting (`core.autocrlf`, `text`, `eol`), a clean filter, `working-tree-encoding` or `ident` applies, status reads the file and decides by content. So a file rewritten with only different line endings, which cleans to the staged content, is clean, and its index entry is refreshed. `git status` may show such a file as ` M` until its index is refreshed, while `git diff` already shows nothing.

Mode changes the checkout cannot express are not reported: with `core.filemode=false` the executable bit is ignored, and with `core.symlinks=false` a symlink checked out as a plain file holding its target is compared by content, so it shows up only when the target text changes.

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

On a local branch with no remote, when you are behind another local branch:

```console
$ grit status
On old  ·  1 behind main

Nothing to commit — working tree clean.

→ grit merge main
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
| `embedded_repositories` | array | Untracked nested repositories that would be recorded as gitlinks if staged. Omitted when empty. Human and `--markdown` output print a one-line **note** on stderr for each path. |
| `clean` | boolean | `true` when there is nothing to commit and nothing untracked. |
| `merging` | boolean | **Deprecated.** Prefer `operation`. `true` when a merge is in progress (`MERGE_HEAD` exists). |
| `in_progress` | array | **Deprecated.** Prefer `operation`. Stable operation ids while work is paused (for example `merge`, `rebase`). Omitted when empty. |
| `conflicts` | array | Unmerged paths. Each item is `{ "path": "<path>", "kind": "<kind>" }`, with a [conflict kind](#conflict-kinds). Omitted when empty. |
| `operation` | object or null | Primary paused operation: `{ "kind", "next_steps", "alternatives" }`. `next_steps` is the ordered sequence; `alternatives` lists mutually exclusive options (for example `grit merge --abort`). Steps are `grit` commands, except `git rebase --continue` for a rebase that git started. Omitted when no operation is in progress. |
| `ignored` | array | Ignored paths when `--ignored` was passed; omitted otherwise. |
| `stash_count` | number | Entries on the stash reflog (`0` when empty). |
| `interrupted_rewrite` | string | Recovery of a rebase or history edit that was killed after moving refs: `finished_checkout` (the rewrite stands), `rolled_back` (it was undone) or `pending` (another process holds `.git/index.lock`). Omitted when nothing was pending. |
| `gitbutler_workspace` | object | Only on a [GitButler workspace](#gitbutler-workspaces): `commit` (the workspace commit, which `head` names and `commits` and `ahead` leave out) and `heads`, the merged branch tips in the workspace's parent order, each with `commit`, `branches` (local branches pointing there, sorted; empty when none), `subject` and `ahead` (commits not on `target`, or `null` when counts were skipped). Omitted otherwise. |
| `warnings` | array | Non-fatal worktree access problems while scanning. Each item is `{ "kind", "message" }` (for example `worktree_access_failed` when a directory could not be opened). Omitted when empty. |

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