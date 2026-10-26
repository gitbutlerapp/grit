---
title: grit clone
summary: Copy a remote repository into a new directory.
group: Getting started
order: 2
---

## Synopsis

```text
grit clone <url> [<dir>] [--depth <N>] [-b|--branch <NAME>] [--blobs <none|SIZE>] [--shallow] [--single-branch | --no-single-branch] [--no-tags] [-o <name>] [--bare] [--no-checkout] [-q|--quiet]
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

On a case-insensitive filesystem (`core.ignorecase`), when the repository contains paths that differ only by case (or other spellings that map to one on-disk file), grit prints a **warning** on **stderr** (human mode only) listing every colliding index path. Only one path from each group exists on disk; the summary **files** and **worktree_bytes** counts reflect distinct on-disk files, not the number of index entries. With `commit.autoStage` (the default), grit does not auto-stage collision losers — their index blobs stay as recorded in the tree even though the work tree shows them as modified.

For private GitHub repositories over HTTPS, sign in first with [`grit auth`](../auth/).

When the remote has no commits yet (an empty bare repository), grit still creates the destination directory, configures `origin`, and points `HEAD` at the remote's default branch without checking out files. Human output includes a short note on stderr (`note: the remote repository is empty.`); `--json` reports `"files": 0`, `"no_checkout_reason": "empty_remote"`, and the configured branch name.

When the remote advertises a default branch that has no commits yet but other branches (or tags) were fetched, grit configures `HEAD` to that default branch without checking out files — the same situation as `git clone` when `HEAD` on the server points at an unborn branch. Human output prints a **warning** on stderr that the default branch has no commits, lists remote branches that do have commits, and may suggest `grit switch <branch>`. `--json` sets `"no_checkout_reason": "unborn_default_branch"` and `"branches_with_commits"` to those branch names; `"files"` stays `0` until you switch.

Pass `--depth <N>` for the same result as `git clone --depth`: only one ref is fetched (single-branch), with a `.git/shallow` boundary at its tip. Without `-b` / `--branch`, that ref is the remote's default branch. With `-b`, it is the named branch or tag (branches resolve under `refs/heads/` first, then `refs/tags/`). Tag checkouts detach `HEAD` at the peeled commit and do not create branch config. Other branches are not downloaded until you fetch them explicitly. Later [`grit fetch`](../fetch/) respects the shallow boundary.

`--shallow` is a hidden alias for `--depth 1` and cannot be combined with `--depth`.

### Partial clone (`--blobs`)

`--blobs none` creates a **blobless** (promisor) clone: commits and trees are present locally, but blob objects are omitted from the initial fetch and fetched on demand later (same idea as `git clone --filter=blob:none`). Grit writes Git-compatible promisor configuration (`remote.<name>.partialclonefilter`, `extensions.partialclone`, and related keys) so the system `git` command can lazy-fetch missing blobs through the same remote.

`--blobs <SIZE>` omits blobs whose inflated size is at or above the limit. `<SIZE>` accepts the same suffixes as Git's `blob:limit` filter (`512`, `1k`, `2m`, …). Smaller blobs stay in the first pack, which keeps checkouts working when only large assets are skipped.

Partial clone trades disk and bandwidth on the first fetch for extra round-trips when something needs blob bytes (for example `git log -p` or reading a large file). It works with `--depth`, `-b` / `--branch`, and the other clone options. The server must allow filter requests (`uploadpack.allowFilter` on bare repos served over smart HTTP or `file://`).

`--bare` creates a bare repository at the destination (the directory itself is the git directory, named with a `.git` suffix when the URL is derived automatically), fetches `refs/heads/*` and tags directly, and configures only `remote.<name>.url` — matching `git clone --bare`. `--no-checkout` fetches and sets `HEAD` to the default branch without writing the index or work tree files, matching `git clone --no-checkout`.

After `--no-checkout`, use [`grit switch`](../switch/) to materialize the revision you need, for example `grit -C client switch --detach HEAD~1 --json`. The first switch writes the entire destination tree, including when you select the current branch or `HEAD`. Files you created at destination paths are protected by the usual untracked-file collision checks. `switch --dry-run` previews how many paths will be written without creating an index or changing files or refs.

You can also clone from a shallow repository (including one created with `git clone --depth`); grit copies the shallow boundary into the new repository so history matches the source.

Working-tree materialization after fetch uses the same parallel checkout path as [`grit switch`](../switch/). Configure [`checkout.workers`](../../library/parallelism.md) and `checkout.thresholdForParallelism` in the repository config. Global [`--threads`](../global-options.md) overrides checkout workers on non-Windows hosts; on **Windows**, set `checkout.workers` explicitly (see [parallelism](../../library/parallelism.md)).

## Options

| Option | Description |
| --- | --- |
| `<url>` | The repository to clone: a URL or a local path. |
| `<dir>` | The directory to clone into. Defaults to the repository's name. |
| `--depth <N>` | Shallow clone: fetch at most `N` commits on the selected ref (`git clone --depth`). |
| `-b`, `--branch <NAME>` | Branch or tag to check out instead of the remote default. |
| `--shallow` | Hidden alias for `--depth 1` (conflicts with `--depth`). |
| `--single-branch` | Fetch only the branch that is checked out (`git clone --single-branch`). |
| `--no-single-branch` | With `--depth`, fetch every branch tip (`git clone --no-single-branch --depth=1`). |
| `--no-tags` | Do not fetch tags; sets `remote.<name>.tagOpt=--no-tags` like Git. |
| `-o`, `--origin <name>` | Name of the remote (default `origin`). |
| `--blobs <none\|SIZE>` | Partial clone: omit all blobs (`none`) or blobs at/above `<SIZE>` (`1k`, `512`, …). |
| `--bare` | Bare repository; no working tree (`git clone --bare`). |
| `--no-checkout` | Fetch refs but do not check out files (`git clone --no-checkout`). |
| `-q`, `--quiet` | No progress, no `Cloning into` line and no human summary. Warnings (such as an empty remote) still go to stderr; `--json` and `--markdown` output is unchanged. |

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
$ grit clone --depth 1 https://github.com/BurntSushi/ripgrep.git
```

Clone a specific branch with limited history:

```console
$ grit clone --depth 3 -b feature file:///srv/git/project.git
```

Clone and detach at a tag:

```console
$ grit clone -b v1.0.0 https://github.com/BurntSushi/ripgrep.git
```

## JSON output

Schema: [v3](/schemas/clone/v3.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `url` | string | The URL or path that was cloned. |
| `path` | string | Directory the clone was created in. |
| `remote` | string | Remote name configured for the source (`origin` by default). |
| `single_branch` | boolean | Whether only one branch was fetched. |
| `tag_fetch` | string | Tag fetch mode for the initial fetch: `all`, `following`, or `none`. Named `tag_fetch` because `tags` is already the count of tag refs fetched. |
| `branch` | string or null | Branch checked out; `null` when detached at a tag. |
| `detached` | boolean | True when `HEAD` is detached (tag checkout). |
| `tag` | string or omitted | Tag name when `-b` named a tag. |
| `depth` | integer or omitted | Shallow depth when `--depth` or `--shallow` was used. |
| `blobs` | string or integer or omitted | `"none"` or a byte limit when `--blobs` was used. |
| `partial` | boolean or omitted | `true` when `--blobs` configured a promisor partial clone. |
| `objects` | integer | Objects installed during fetch: for network clones, objects in the received pack; for local-path clones, objects in newly linked pack indexes plus any reachable loose objects hardlinked from the source. |
| `bytes` | integer | On-disk payload bytes installed during fetch: for network clones, the received `.pack` size; for local-path clones, the sum of newly linked `.pack` files plus reachable loose object files (same bytes as hardlinked/copied object store data). |
| `branches` | integer | Remote branches fetched. |
| `tags` | integer | Remote tags fetched. |
| `files` | integer | Working-tree files checked out (distinct on-disk paths when `core.ignorecase` collapsed spellings). |
| `path_collisions` | array of strings | Index paths that collided during checkout on a case-insensitive filesystem. Omitted when empty. |
| `object_types` | object or null | Objects received by type: `commits`, `trees`, `blobs` and `tags` (annotated tag objects). Populated when grit indexed the received pack or can tally every object in the transfer. `null` when a local pack link installed more objects than the transfer summary tracks individually (typical for a single linked pack on clone). |
| `worktree_bytes` | integer | Total size of the checked-out files. |
| `seconds` | number | Wall-clock seconds for the whole clone. |
| `phases` | array | Time spent in each step, in order: `phase` (`receiving`, `indexing`, `resolving_deltas`, `copying` or `checkout`), `unit` (`bytes`, `objects` or `files`), `count` and `seconds`. Steps that processed nothing are left out. |
| `bare` | boolean | `true` when `--bare` was used. |
| `no_checkout_reason` | string or omitted | When no files were checked out: `empty_remote`, `unborn_default_branch`, `bare`, or `no_checkout`. Omitted when checkout succeeded. |
| `branches_with_commits` | array of strings | Remote branch names that have commits when `no_checkout_reason` is `unborn_default_branch`. Omitted when empty. |

Example (repository with commits):

```json
{
  "url": "/srv/git/project.git",
  "path": "project",
  "remote": "origin",
  "single_branch": false,
  "tag_fetch": "all",
  "branch": "main",
  "detached": false,
  "bare": false,
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
  "remote": "origin",
  "single_branch": false,
  "tag_fetch": "all",
  "branch": "main",
  "detached": false,
  "bare": false,
  "objects": 0,
  "bytes": 0,
  "branches": 0,
  "tags": 0,
  "files": 0,
  "no_checkout_reason": "empty_remote",
  "object_types": null,
  "worktree_bytes": 0,
  "seconds": 0.013,
  "phases": []
}
```

Unborn remote default branch (`HEAD` on the server points at a branch with no commits, but `main` was pushed):

```json
{
  "url": "/tmp/remote.git",
  "path": "clone2",
  "remote": "origin",
  "single_branch": false,
  "tag_fetch": "all",
  "branch": "master",
  "detached": false,
  "bare": false,
  "objects": 3,
  "bytes": 458,
  "branches": 1,
  "tags": 0,
  "files": 0,
  "no_checkout_reason": "unborn_default_branch",
  "branches_with_commits": ["main"],
  "object_types": null,
  "worktree_bytes": 0,
  "seconds": 0.003,
  "phases": [
    {
      "phase": "copying",
      "unit": "objects",
      "count": 1,
      "seconds": 0.00006
    }
  ]
}
```

## Markdown output

Pass [`--markdown`](../global-options/) for agent-friendly output.

Schema: [v3](/schemas/clone/v3.json)

Markdown uses headings, bullet lists, and fenced blocks; pipe characters inside table cells are escaped. See the JSON output section for the stable field contract.



```text
## Clone complete

- **path**: `cloned-markdown`
- **branch**: `main`
- **url**: `[UPSTREAM]`
- **objects**: 8
- **object types**: 2 commits, 2 trees, 3 blobs, 1 tags
- **bytes**: 685 ([SIZE])
- **branches**: 2
- **tags**: 1
- **files**: 2
- **worktree bytes**: 23 ([SIZE])
- **elapsed**: [DURATION]

| Phase | Count | Unit | Seconds |
| --- | ---: | --- | ---: |
| copying | 8 | objects | [SECONDS] |
| checkout | 2 | files | [SECONDS] |
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Clone succeeded (including empty remote with no checkout). |
| `1` | Destination not empty, checkout failure, or other error. Missing or refused credentials are `kind` `auth_required` / `auth_rejected` with a `hint` in `--json` and on the human `hint:` line. HTTPS credentials come from your credential helper, then an askpass program (`GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`); see [Credentials](../agents/#credentials-https). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| `128` | Could not reach or read the source: missing path, DNS or connection failure, HTTP error status ([`remote_unreadable`](../exit-codes/)); a `404` adds a hint to check the URL. |

See [Exit codes](../exit-codes/).


## See also

[grit init](../init/), [grit remote](../remote/), [grit fetch](../fetch/), [grit auth](../auth/)