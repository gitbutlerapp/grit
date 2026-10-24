---
title: grit add
summary: Stage changes. With no paths, stages everything.
group: Making changes
order: 1
---

## Synopsis

```text
grit add [-u | --update] [--dry-run] [--hunks <id,...>] [--lines <path:start-end>] [-U<n> | --unified <n>] [--patch-file <file|->] [--executable | --non-executable] [--pathspec-from-file <file|->] [--pathspec-file-nul] [<path>...]
```

## Description

Stages changes so they show up under **Staged** in [`grit status`](../status/). With no paths, `grit add` stages everything `grit status` reports: modified files, deleted files and untracked files. With paths, it stages only those files, or everything under those directories.

Paths are relative to your current directory, so from inside `src/`, `grit add main.rs` stages `src/main.rs`. Paths like `../README.md` and absolute paths inside the repository work too. A path that matches nothing is an error, and nothing is staged. A path that crosses a symlinked directory (for example `link/file` when `link` is a symlink) is an error: nothing is staged. Naming a path that exists but is excluded by `.gitignore`, `.git/info/exclude`, or `core.excludesfile` is also an error: nothing is staged and the command lists the ignored paths (pathless `grit add` still skips ignored files silently, like Git). Explicitly staging an **unmerged** path fails while Git-style conflict markers remain in the worktree file (the same rule as [`grit commit`](../commit/) auto-staging); remove the markers or use a resolved file, then run `grit add` again.

On filesystems where Grit sets `core.filemode=false` (typical on Windows), every new file is staged as non-executable (`100644`) even when it is a script. Use `--executable` on the paths you ship as runnable scripts so the index and commits record mode `100755`. Use `--non-executable` to clear an explicit executable bit in the index. These flags require at least one path argument.

[`grit commit`](../commit/) stages every change before it records a commit, so you don't need `grit add` to commit. It's useful for starting to track a new file and checking what will be committed.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to stage. Omit to stage every change. |
| `-u`, `--update` | Stage tracked changes only (modified and deleted paths). Untracked files are left alone. With no paths, applies to the whole tree. |
| `--dry-run` | List paths that would be staged without updating the index or writing objects. |
| `--hunks` | Stage only the listed hunk ids from `grit diff --json` (`hunks[].id`). Comma-separated ids; pass the flag again to add more. Optional `-- paths` narrow the diff scan. Cannot be combined with `--patch-file`, `-u` / `--update`, or `-N` / `--intent-to-add`. |
| `--lines` | Stage only new-side line ranges from `grit diff --json` (`lines[].new`). Use `path:start-end` or `path:line` (1-based, inclusive); comma-separate or repeat the flag. Mix with `--hunks`. Same restrictions as `--hunks`. |
| `-U`, `--unified` | With `--hunks` only: unified context lines for hunk boundaries (same meaning as `grit diff -U`; default 3 when omitted). Use the same value as the diff that produced the ids. |
| `--patch-file` | Apply a unified diff to the index only (like `git apply --cached`). Use `-` for stdin. Cannot be combined with `--hunks`, paths, `-u` / `--update`, or `-N` / `--intent-to-add`. |
| `-N`, `--intent-to-add` | Record intent-to-add entries (empty blob, extended flag) instead of hashing content. |
| `-f`, `--force` | Stage ignored paths when named explicitly (instead of erroring with pathspec ignored). |
| `--pathspec-from-file` | Read additional pathspecs from a file, or `-` for stdin (same rules as Git). |
| `--pathspec-file-nul` | With `--pathspec-from-file`, entries are NUL-separated instead of line-based. |
| `--executable` | Stage listed regular files with mode `100755` in the index (requires at least one path). |
| `--non-executable` | Stage listed regular files with mode `100644` in the index (requires at least one path). |

## Examples

Stage everything:

```console
$ grit add
Staged 3 changes.
```

Stage one file and a directory:

```console
$ grit add README.md src/
Staged 2 changes.
```

Apply a patch to the index (worktree files stay as-is until you edit them):

```console
$ grit add --patch-file changes.diff
Staged 1 change.
```

Stage selected hunks by stable id (from `grit diff --json`):

```console
$ grit diff --unstaged --json -- f.txt | jq -r '.files[0].hunks[0].id'
a1b2c3d4e5f6
$ grit add --hunks a1b2c3d4e5f6 -- f.txt
Staged 1 change.
```

Stage a line range within one hunk:

```console
$ grit add --lines f.txt:10-15 -- f.txt
Staged 1 change.
```

## JSON output

Schema: [v1](/schemas/add/v1.json)

Pass `--json` for stable, scripting-friendly output.

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `added` | array | Newly staged paths. |
| `modified` | array | Staged modifications. |
| `removed` | array | Staged deletions. |
| `intent_to_add` | array | Paths recorded with intent-to-add. |
| `dry_run` | boolean | Whether the index was left unchanged. |
| `count` | number | Total paths touched. |
| `staged` | number | Same as `count` for successful staging. |
| `staged_hunks` | object | Path → hunk ids when `--hunks` staged something (omitted when empty). |
| `staged_lines` | object | Path → line ranges when `--lines` staged something (omitted when empty). |

```json
{
  "added": ["src/new.rs"],
  "modified": ["README.md"],
  "removed": [],
  "intent_to_add": [],
  "dry_run": false,
  "count": 2,
  "staged": 2
}
```

When `--hunks` or `--lines` staged something, `staged_hunks` or `staged_lines` maps path → ids or ranges appear on stdout; they are omitted when empty.

Grit computes blob object ids with the same plain SHA-1 digests as Git. SHAttered test vectors (for example blobs in the `git/git` history) stage and commit normally; grit does not reject them during ingestion.

## Markdown output

Schema: [v1](/schemas/add/v1.json)

Pass [`--markdown`](../global-options/) for an `Add` heading and how many paths were staged:

```text
- Staged **1** change
```

When there is nothing to stage and you ran `grit add` with no paths:

```text
Nothing to stage — working tree clean.
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.

## See also

[grit status](../status/), [grit commit](../commit/)
