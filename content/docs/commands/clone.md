---
title: grit clone
summary: Copy a remote repository into a new directory.
group: Getting started
order: 2
---

## Synopsis

```text
grit clone <url> [<dir>] [--shallow]
```

## Description

Copies the repository at `<url>` into a new directory and checks out the remote's default branch. The remote is saved as `origin`, so [`grit fetch`](../fetch/), [`grit pull`](../pull/) and [`grit push`](../push/) work with no further setup.

Clone fetches every remote tag under `refs/tags/`, including tags on commits that are not reachable from any branch tip (matching `git clone`). Later [`grit fetch`](../fetch/) uses tag-following by default; pass `--tags` to fetch all tags again.

`<url>` can be an `https://` or `http://` URL, an `ssh://` URL or `user@host:path` address, a `git://` URL, a `file://` URL, a path to a local repository, or a path to a [git bundle](https://git-scm.com/docs/git-bundle) file (recognized by its header signature). A local path is stored as an absolute path so that later fetches work from anywhere.

Without `<dir>`, the directory is named after the last path component of the URL with any `.git` suffix removed (splitting on both `/` and `\`, so Windows paths like `..\..\origin.git` clone into `origin`), so `https://github.com/gitbutlerapp/grit.git` clones into `grit`. The destination must not exist or must be empty. If the path already exists and is not empty, grit refuses the clone and leaves the directory and its files unchanged.

On an interactive terminal (human output, not `--json` or `--markdown`), each step of the clone gets one live line on stderr: **Receiving** (pack bytes), **Indexing** (objects), **Resolving** (deltas) and **Checkout** (files). Each line has a scrolling, colored graph of throughput over the last few seconds, a count, a completion bar when the total is known, the current rate and the elapsed time. When a step finishes, its line stays on screen with a graph of the whole step, the total and the average rate.

While the pack streams in, grit counts its objects as they arrive (the total comes from the pack header), so the **Receiving** line shows `received/total` objects and a completion bar, as Git's does. Under the live line, an **Overall** bar estimates how far the whole clone is and how long is left:

```text
⠸ Receiving   253,612/421,673  ━━━━━━────  60%         34.2 MiB/s    7.5s
  Overall    ━━━━━━━━━━━━━━━━━━━━━━──────────────────   53%  ~8s left
```

The step under way is projected from its own pace so far. Steps that have not started are sized from the pack's object and delta counts at typical indexing, delta and checkout rates. Before any objects arrive (while the remote is still preparing the pack), the bar shows `estimating`. The percentage never goes down, but the time left can grow when objects slow down, for example when large blobs follow many small commits. The overall bar needs an ANSI terminal; it is cleared once the clone finishes, before the summary.

After the clone, stdout gets a summary: total time with a bar showing where it went (including time spent connecting before any data arrives and finishing after the last step), bytes received with average and peak throughput, objects by type, branches and tags, and the files and bytes in the work tree. Colors follow the terminal and are turned off by `NO_COLOR`.

For private GitHub repositories over HTTPS, sign in first with [`grit auth`](../auth/).

When the remote has no commits yet (an empty bare repository), grit still creates the destination directory, configures `origin`, and points `HEAD` at the remote's default branch without checking out files. Human output includes a short note on stderr (`note: the remote repository is empty.`); `--json` reports `"files": 0`, `"no_checkout_reason": "empty_remote"`, and the configured branch name.

When the remote advertises a default branch that has no commits yet but other branches (or tags) were fetched, grit configures `HEAD` to that default branch without checking out files — the same situation as `git clone` when `HEAD` on the server points at an unborn branch. Human output prints a **warning** on stderr that the default branch has no commits, lists remote branches that do have commits, and may suggest `grit switch <branch>`. `--json` sets `"no_checkout_reason": "unborn_default_branch"` and `"branches_with_commits"` to those branch names; `"files"` stays `0` until you switch.

Pass `--shallow` for the same result as `git clone --depth=1`: only the remote's default branch is fetched (single-branch), with a `.git/shallow` boundary at its tip. Other branches are not downloaded until you fetch them explicitly. Later [`grit fetch`](../fetch/) respects the shallow boundary.

You can also clone from a shallow repository (including one created with `git clone --depth`); grit copies the shallow boundary into the new repository so history matches the source.

Working-tree materialization after fetch uses the same parallel checkout path as [`grit switch`](../switch/). Configure [`checkout.workers`](../../library/parallelism.md) and `checkout.thresholdForParallelism` in the repository config, or pass global [`--threads`](../global-options.md) to override worker count for this process.

### Exit status

On failure, `grit clone` exits **1** for most errors (destination not empty, checkout failure, and so on). When the remote cannot be read — for example SSH cannot connect or the repository path on the server is missing — grit exits **128** with `could not read from remote repository`. That is distinct from a reachable empty remote, which exits **0** after configuring `origin` and the default branch without checking out files (see above).

If a large receive fails with `received pack file missing` pointing at a `pack-receive-*.tmp` path under `.git/objects/incoming/`, real-time antivirus or disk cleanup may have removed the in-progress pack before index-pack ran; retry the clone or check that nothing else is deleting files under the repository directory during the receive.

## Options

| Option | Description |
| --- | --- |
| `<url>` | The repository to clone: a URL or a local path. |
| `<dir>` | The directory to clone into. Defaults to the repository's name. |
| `--shallow` | Shallow clone of the default branch only (`git clone --depth=1`). |

## Examples

Clone a repository from GitHub:

```console
$ grit clone https://github.com/BurntSushi/ripgrep.git
Cloning https://github.com/BurntSushi/ripgrep.git → ripgrep
  remote: Enumerating objects: 14062, done.
  remote: Counting objects: 100% (83/83), done.
  remote: Compressing objects: 100% (47/47), done.
  remote: Total 14062 (delta 51), reused 36 (delta 36), pack-reused 13979 (from 2)
✓ Receiving  ▃▃▃▃▆▆▆███▆▆▆▆▆▆▆▆▆▆▆▆▇▇▇▅▅▅          5.67 MiB         13.0 MiB/s   435ms
✓ Indexing   ▆▆▆▆▆▆▆▆▆▆▆▆▆▆██████████████    14,062 objects   343.1k objects/s    40ms
✓ Resolving  ████████████████████████████      9,840 deltas    370.3k deltas/s    26ms
✓ Checkout   ████████████████████████████         237 files       4.6k files/s    51ms

✓ Cloned into ripgrep on master in 1.6s

  time      ················███████████·▓▒·········░  1.6s
            · connect 651ms  █ receiving 435ms  ▓ indexing 40ms  ▒ resolving 26ms  · other 359ms  ░ checkout 51ms
  data      5.67 MiB received · 13.0 MiB/s avg · 20.1 MiB/s peak
  objects   14,062 · 2,327 commits · 6,351 trees · 5,112 blobs · 272 tags
  refs      16 branches · 272 tags
  files     237 files · 3.11 MiB in the work tree
```

Progress lines are written to stderr and update in place on a terminal; the time bar and legend are colored by step there. When stderr is not a TTY (for example in a CI log or with stderr redirected), only the initial `Cloning into …` line is written to stderr, and the summary still appears on stdout.

Clone into a directory with a different name:

```console
$ grit clone git@github.com:gitbutlerapp/grit.git grit-src
```

Clone a local repository:

```console
$ grit clone /srv/git/project.git
```

Shallow clone (single commit of history on the default branch):

```console
$ grit clone --shallow https://github.com/BurntSushi/ripgrep.git
```

## JSON output

Schema: [v1](/schemas/clone/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The URL or path that was cloned. |
| `path` | string | Directory the clone was created in. |
| `branch` | string | Branch that was checked out. |
| `objects` | integer | Objects received in the fetch pack. |
| `bytes` | integer | Size in bytes of the received pack. |
| `branches` | integer | Remote branches fetched. |
| `tags` | integer | Remote tags fetched. |
| `files` | integer | Working-tree files checked out. |
| `object_types` | object or null | Objects received by type: `commits`, `trees`, `blobs` and `tags` (annotated tag objects). `null` when the pack was linked or copied from a local repository without being re-indexed. |
| `worktree_bytes` | integer | Total size of the checked-out files. |
| `seconds` | number | Wall-clock seconds for the whole clone. |
| `phases` | array | Time spent in each step, in order: `phase` (`receiving`, `indexing`, `resolving_deltas`, `copying` or `checkout`), `unit` (`bytes`, `objects` or `files`), `count` and `seconds`. Steps that processed nothing are left out. |
| `no_checkout_reason` | string or omitted | When no files were checked out: `empty_remote` (nothing on the remote) or `unborn_default_branch` (default branch unborn but other refs may exist). Omitted when checkout succeeded. |
| `branches_with_commits` | array of strings | Remote branch names that have commits when `no_checkout_reason` is `unborn_default_branch`. Omitted when empty. |

Example (repository with commits):

```json
{
  "url": "/srv/git/project.git",
  "path": "project",
  "branch": "main",
  "objects": 42,
  "bytes": 8192,
  "branches": 1,
  "tags": 0,
  "files": 3,
  "object_types": {
    "commits": 4,
    "trees": 9,
    "blobs": 29,
    "tags": 0
  },
  "worktree_bytes": 6144,
  "seconds": 0.21,
  "phases": [
    { "phase": "copying", "unit": "objects", "count": 42, "seconds": 0.004 },
    { "phase": "checkout", "unit": "files", "count": 3, "seconds": 0.002 }
  ]
}
```

Empty remote (no branch tips yet):

```json
{
  "url": "/tmp/origin.git",
  "path": "project",
  "branch": "main",
  "objects": 0,
  "bytes": 0,
  "branches": 0,
  "tags": 0,
  "files": 0,
  "no_checkout_reason": "empty_remote",
  "object_types": null,
  "worktree_bytes": 0,
  "seconds": 0.05,
  "phases": []
}
```

Unborn remote default branch (`HEAD` on the server points at a branch with no commits, but `main` was pushed):

```json
{
  "url": "/tmp/remote.git",
  "path": "clone2",
  "branch": "master",
  "objects": 4,
  "bytes": 72896,
  "branches": 1,
  "tags": 0,
  "files": 0,
  "no_checkout_reason": "unborn_default_branch",
  "branches_with_commits": ["main"],
  "object_types": { "commits": 1, "trees": 1, "blobs": 1, "tags": 0 },
  "worktree_bytes": 0,
  "seconds": 0.12,
  "phases": []
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v1](/schemas/clone/v1.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.

## See also

[grit init](../init/), [grit remote](../remote/), [grit fetch](../fetch/), [grit auth](../auth/)
