---
title: grit add
summary: Stage changes. With no paths, stages everything.
group: Making changes
order: 1
---

## Synopsis

```text
grit add [--executable | --non-executable] [--pathspec-from-file <file|->] [--pathspec-file-nul] [<path>...]
```

## Description

Stages changes so they show up under **Staged** in [`grit status`](../status/). With no paths, `grit add` stages everything `grit status` reports: modified files, deleted files and untracked files. With paths, it stages only those files, or everything under those directories.

Paths are relative to your current directory, so from inside `src/`, `grit add main.rs` stages `src/main.rs`. Paths like `../README.md` and absolute paths inside the repository work too. A path that matches nothing is an error, and nothing is staged. A path that crosses a symlinked directory (for example `link/file` when `link` is a symlink) is an error: nothing is staged. Naming a path that exists but is excluded by `.gitignore`, `.git/info/exclude`, or `core.excludesfile` is also an error: nothing is staged and the command lists the ignored paths (pathless `grit add` still skips ignored files silently, like Git).

On filesystems where Grit sets `core.filemode=false` (typical on Windows), every new file is staged as non-executable (`100644`) even when it is a script. Use `--executable` on the paths you ship as runnable scripts so the index and commits record mode `100755`. Use `--non-executable` to clear an explicit executable bit in the index. These flags require at least one path argument.

[`grit commit`](../commit/) stages every change before it records a commit, so you don't need `grit add` to commit. It's useful for starting to track a new file and checking what will be committed.

## Options

| Option | Description |
| --- | --- |
| `<path>...` | Files or directories to stage. Omit to stage every change. |
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

Mark a script executable when the filesystem does not track the execute bit:

```console
$ grit add --executable scripts/deploy.sh
Staged 1 change.
```

## JSON output

Schema: [v1](/schemas/add/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `staged` | number | How many paths were staged by this invocation. |

Example:

```json
{
  "staged": 2
}
```

When staging would hash blob data that triggers an SHA-1 chosen-prefix collision, the command fails (exit **1**). With `--json`, stdout is the shared [error object](../scripting.md) with `kind` `hash_collision` and `oid` set to the unsafe digest hex (see [error schema](/schemas/error/v1.json)).

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

## See also

[grit status](../status/), [grit commit](../commit/)
