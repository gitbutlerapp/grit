---
title: grit clone
summary: Copy a remote repository into a new directory.
group: Getting started
order: 2
---

## Synopsis

```text
grit clone <url> [<dir>] [--depth <N>] [-b|--branch <NAME>] [--blobs <none|auto|SIZE>] [--config <KEY=VALUE>]... [--shallow] [--single-branch | --no-single-branch] [--no-tags] [-o <name>] [--bare] [--no-checkout] [-q|--quiet]
```

## Description

Copies the repository at `<url>` into a new directory and checks out the remote's default branch. The remote is saved as `origin`, so [`grit fetch`](../fetch/), [`grit pull`](../pull/) and [`grit push`](../push/) work with no further setup.

Clone fetches every remote tag under `refs/tags/` on a full multi-branch clone, including tags on commits that are not reachable from any branch tip (matching `git clone`). With `--single-branch` (or a shallow clone without `--no-single-branch`), only tags whose peeled commits are reachable from the fetched branch history are imported — the same tag-following default as `git clone --single-branch`. Later [`grit fetch`](../fetch/) uses tag-following by default; pass `--tags` to fetch all tags again.

With files-based ref storage, clone packs imported tags and remote-tracking refs,
including final peel records for nested annotated tags. Symbolic `HEAD` refs stay
loose, and reflogs are preserved. Bare and shallow clones pack the tags they import;
`--no-tags` imports none. This keeps bounded decorated logs from paying for a loose
file and object lookup for every historical tag. Ordinary fetches keep their
existing ref layout. Reftable repositories continue to use their ref backend.


`<url>` can be an `https://` or `http://` URL, an `ssh://` URL or `user@host:path` address, a `git://` URL, a `file://` URL, a path to a local repository, or a path to a [git bundle](https://git-scm.com/docs/git-bundle) file (recognized by its header signature). A local path is stored as an absolute path so that later fetches work from anywhere.

Cloning a plain local path hard-links the source's packfiles, like `git clone --local`, so the new repository also gets objects no ref reaches. A `file://` URL gets only the objects reachable from the cloned refs, like any other transport. Use it when the source holds history you don't want copied, such as deleted branches or hidden refs.

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

When the checked-out tree contains **submodule gitlinks** (mode `160000`), grit creates an empty directory at each path and does **not** fetch the nested repository. There is no `grit submodule` command and no `--recurse-submodules` flag. After a successful checkout, human and `--markdown` output print a one-line **note** on stderr listing how many gitlinks were found; `--json` includes a `submodule_paths` array (omitted when empty). Use the system `git` command if you need submodule checkout or updates.

### Starting from the server's bundle

When an HTTP(S) server offers a prebuilt bundle through Git's `bundle-uri` protocol command (as [`grit serve`](../serve/) does), clone downloads that bundle first, unpacks it, and then fetches only what is newer than it. A bundle is a plain file, so the download resumes instead of starting over:

- a connection that drops is picked up again from where it stopped, a few times, in the same run;
- an interrupted clone (Ctrl-C, a lost network, a closed laptop) keeps what it downloaded in `$XDG_CACHE_HOME/grit/bundles` (`~/.cache/grit/bundles` by default). Run the same `grit clone` again and the download carries on from there.

The bundle download has its own display: an arrow and the bundle's URL, then a bar. A resumed download says where it picked up, and its bar shows the bytes from the earlier run in grey:

```text
↻ Resuming bundle download at 14.1 MiB of 58.5 MiB (24%)  example.com/demo/bundles/1792925548-38bec2f6f89f.bundle
  ━━━━━━━━━━━━━━━━━━━─────────────   60%  35.5 MiB of 58.5 MiB  5.22 MiB/s  ~4s left
```

When it finishes, the line becomes `✓ bundle 58.5 MiB in 8.5s · 5.23 MiB/s · resumed at 14.1 MiB`, and the summary gets a `bundle` row. Shallow (`--depth`) and partial (`--blobs`) clones never use a bundle, and `transfer.bundleURI=false` in config turns bundles off. When the bundle cannot be used, clone warns and fetches everything as usual.

For private GitHub repositories over HTTPS, sign in first with [`grit auth`](../auth/). For other hosts, save a user name and token first with `grit auth <url> --username <user> --token <token>`.

When the remote has no commits yet (an empty bare repository), grit still creates the destination directory, configures `origin`, and points `HEAD` at the remote's default branch without checking out files. Human output includes a short note on stderr (`note: the remote repository is empty.`); `--json` reports `"files": 0`, `"no_checkout_reason": "empty_remote"`, and the configured branch name.

When the remote advertises a default branch that has no commits yet but other branches (or tags) were fetched, grit configures `HEAD` to that default branch without checking out files — the same situation as `git clone` when `HEAD` on the server points at an unborn branch. Human output prints a **warning** on stderr that the default branch has no commits, lists remote branches that do have commits, and may suggest `grit switch <branch>`. `--json` sets `"no_checkout_reason": "unborn_default_branch"` and `"branches_with_commits"` to those branch names; `"files"` stays `0` until you switch.

Pass `--depth <N>` for the same result as `git clone --depth`: only one ref is fetched (single-branch), with a `.git/shallow` boundary at its tip. Without `-b` / `--branch`, that ref is the remote's default branch. With `-b`, it is the named branch or tag (branches resolve under `refs/heads/` first, then `refs/tags/`). Tag checkouts detach `HEAD` at the peeled commit and do not create branch config. Other branches are not downloaded until you fetch them explicitly. Later [`grit fetch`](../fetch/) respects the shallow boundary.

`--shallow` is a hidden alias for `--depth 1` and cannot be combined with `--depth`.

### Partial clone (`--blobs`)

`--blobs none` creates a **blobless** (promisor) clone: commits and trees are present locally, but blob objects are omitted from the initial fetch and fetched on demand later (same idea as `git clone --filter=blob:none`). Grit writes Git-compatible promisor configuration (`remote.<name>.partialclonefilter`, `extensions.partialclone`, and related keys) so the system `git` command can lazy-fetch missing blobs through the same remote.

`--blobs <SIZE>` omits blobs whose inflated size is at or above the limit. `<SIZE>` accepts the same suffixes as Git's `blob:limit` filter (`512`, `1k`, `2m`, …). Smaller blobs stay in the first pack, which keeps checkouts working when only large assets are skipped.

`--blobs auto` takes the filter from the server instead: from the [large-object promisors](#large-object-promisors) it advertises and the clone accepts, using the strictest `partialCloneFilter` they advertise (`blob:none` over any limit, else the smallest limit). Without an accepted remote that advertises a filter, nothing is filtered. The clone records `remote.<name>.partialclonefilter=auto`, so each later fetch picks its filter the same way, as Git 2.54 and newer do; older Git does not understand that value.

Partial clone trades disk and bandwidth on the first fetch for extra round-trips when something needs blob bytes (for example `git log -p` or reading a large file). It works with `--depth`, `-b` / `--branch`, and the other clone options. The server must allow filter requests (`uploadpack.allowFilter` on bare repos served over smart HTTP or `file://`); a server that does not sends every blob, and the summary says the filter was ignored. A partial clone of a local path or `file://` URL runs grit's own upload-pack in-process, where Git would start `git upload-pack`, so it needs no `git` installed and follows the same `uploadpack.*` settings, read from the served repository's config and, as with Git, from your global and system config.

When you pass `--blobs` but the server does not advertise filter support, grit warns on stderr (even with `-q`), fetches an unfiltered pack, and does not treat the clone as partial in human output or `--json` (`partial` is omitted or false). Promisor configuration is still written, matching `git clone --filter` when the server ignores the filter, so later commands may warn about a promisor remote even though every blob arrived in the first pack.

`--config <KEY=VALUE>` writes a config value into the new repository before anything is fetched, like `git clone -c`. It can be repeated, and a key given twice gets both values. A key without `=VALUE` is set to `true`. Unlike the global `-c`, which lasts for one command, these values stay in the clone's config.

### Large-object promisors

A server can keep its large blobs on another remote, a large-object promisor (LOP), and say so with the protocol v2 `promisor-remote` capability (see [grit serve](../serve/#large-object-promisors)). A clone that accepts the LOP lets the server leave those blobs out; they are then fetched from the LOP when something needs them, instead of through the server.

Accepting takes two things in the new repository's config, so they go in with `--config`: the LOP itself as a promisor remote (`remote.<name>.url` and `remote.<name>.promisor=true`), and a policy in `promisor.acceptFromServer`:

| Value | Accepts |
| --- | --- |
| `none` (default) | nothing |
| `knownUrl` | a remote configured under the advertised name with the same URL (the safest choice) |
| `knownName` | a remote configured under the advertised name, whatever its URL |
| `all` | every advertised remote; only for servers you fully trust |

`promisor.checkFields` (`partialCloneFilter`, `token`) also requires the advertised value of each listed field to equal the one configured for the remote. `promisor.storeFields` copies the advertised values of the listed fields into the config of a remote that is already configured; it never creates a remote or changes its URL. Lazy fetches always use the URL configured locally, never the advertised one.

Accepting needs a partial clone (`--blobs`): a full clone needs every object from the server, so it declines (`not_partial`). The clone's summary says what it decided:

```console
$ grit clone --blobs auto \
    --config remote.lop.url=https://lop.example.com/project.git \
    --config remote.lop.promisor=true \
    --config promisor.acceptFromServer=knownUrl \
    https://git.example.com/project.git
Cloning into 'project' ...
✓ Cloned into project on main in 128ms
  partial  partial clone; large blobs are fetched on demand
  promisor  lop accepted · blob:limit=5k

  time      ·······················█▓······▒▒▒▒▒▒▒▒▒  128ms
            · connect 72ms  █ receiving 0ms  ▓ indexing 2ms  · other 17ms  ▒ checkout 28ms
  data      225 B received
  objects   3 · 1 commit · 1 tree · 1 blob
  refs      1 branch · 0 tags
  files     2 files · 19.5 KiB in the work tree
```

The checkout's missing blobs came from `lop`. Missing objects are always asked of the promisor remotes in config order, with the remote the clone came from (`extensions.partialClone`) last, so a LOP configured before it is tried first and the server is only asked for what the LOP did not have. A server refuses a request that names any object it lacks, so when the LOP holds only the large blobs, the missing objects are split into smaller requests until each remote is asked only for what it has. When an object cannot be fetched, the error names every remote that failed and why.

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
| `--single-branch` | Fetch only the branch that is checked out and follow tags into that history only (`git clone --single-branch`). JSON `tag_fetch` is `following`. |
| `--no-single-branch` | With `--depth`, fetch every branch tip (`git clone --no-single-branch --depth=1`). |
| `--no-tags` | Do not fetch tags; sets `remote.<name>.tagOpt=--no-tags` like Git. |
| `-o`, `--origin <name>` | Name of the remote (default `origin`). |
| `--blobs <none\|auto\|SIZE>` | Partial clone: omit all blobs (`none`), blobs at/above `<SIZE>` (`1k`, `512`, …), or what the accepted promisor remotes hold (`auto`). |
| `--config <KEY=VALUE>` | Write a config value into the new repository before fetching (`git clone -c`). Repeatable. |
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
| `blobs` | string or integer or omitted | The filter used: `"none"` or a byte limit. With `--blobs auto`, the filter it resolved to; omitted when nothing was filtered. |
| `partial` | boolean or omitted | `true` when the server applied the `--blobs` filter on the initial fetch. Omitted or false when the server lacks filter support (see warning above). |
| `filter_auto` | boolean or omitted | `true` with `--blobs auto`. |
| `filter_ignored` | boolean or omitted | `true` when the server does not support filters, so `--blobs` was not sent and every blob was downloaded (`partial` is then omitted); the clone is still set up as a partial clone, as Git leaves it. The human summary then shows a `filter` line saying so instead of calling the clone blobless. |
| `promisor_remotes` | array or omitted | The promisor remotes the server advertised: `name`, `url` (credentials removed), `accepted`, `filter` (the advertised `partialCloneFilter`, or `null`) and `rejected` (why it was not accepted: `policy_none`, `unknown_remote`, `url_mismatch`, `field_missing`, `field_mismatch` or `not_partial`; `null` when accepted). Omitted when the server advertised none. Tokens are never shown. |
| `stored_fields` | array or omitted | Fields copied into config by `promisor.storeFields`: `remote` and `field` (`partialCloneFilter` or `token`), never the value. Omitted when nothing was stored. |
| `promisor_warnings` | array or omitted | Promisor config or advertised values that were ignored: `code` is `unknown_accept_value` (`promisor.acceptFromServer` is not a known policy, so nothing is accepted), `unknown_field` (a `promisor.checkFields` / `promisor.storeFields` entry is not a known field), `invalid_advertised_value` (an advertised field was not stored because its value is invalid) or `unusable_auto_filter` (`--blobs auto` cannot use an accepted remote's filter, such as `tree:0`); `key`, `remote`, `field` and `value` say which, when they apply. A rejected advertised value is never shown. Shown as `promisor  warning: …` lines in the human output and `promisor warning` bullets in `--markdown`. Omitted when there were none. |
| `objects` | integer | Objects installed during fetch: for network clones, objects in the received pack; for local-path clones, objects in newly linked pack indexes plus any reachable loose objects hardlinked from the source. |
| `bytes` | integer | On-disk payload bytes installed during fetch: for network clones, the received `.pack` size; for local-path clones, the sum of newly linked `.pack` files plus reachable loose object files (same bytes as hardlinked/copied object store data). |
| `branches` | integer | Remote branches fetched. |
| `tags` | integer | Remote tags fetched. |
| `files` | integer | Working-tree files checked out (distinct on-disk paths when `core.ignorecase` collapsed spellings). |
| `path_collisions` | array of strings | Index paths that collided during checkout on a case-insensitive filesystem. Omitted when empty. |
| `submodule_paths` | array of strings | Gitlink paths in the checked-out tree. Grit does not fetch submodule repositories. Omitted when empty. |
| `object_types` | object or null | Objects received by type: `commits`, `trees`, `blobs` and `tags` (annotated tag objects). Populated when grit indexed the received pack or can tally every object in the transfer. `null` when a local pack link installed more objects than the transfer summary tracks individually (typical for a single linked pack on clone). |
| `worktree_bytes` | integer | Total size of the checked-out files. |
| `seconds` | number | Wall-clock seconds for the whole clone. |
| `phases` | array | Time spent in each step, in order: `phase` (`bundle`, `receiving`, `indexing`, `resolving_deltas`, `copying` or `checkout`), `unit` (`bytes`, `objects` or `files`), `count` and `seconds`. Steps that processed nothing are left out. |
| `bare` | boolean | `true` when `--bare` was used. |
| `no_checkout_reason` | string or omitted | When no files were checked out: `empty_remote`, `unborn_default_branch`, `bare`, or `no_checkout`. Omitted when checkout succeeded. |
| `branches_with_commits` | array of strings | Remote branch names that have commits when `no_checkout_reason` is `unborn_default_branch`. Omitted when empty. |
| `bundle` | object or omitted | The server's prebuilt bundle the clone started from: `uri`, `bytes` (its size) and `resumed_from` (bytes an earlier, interrupted clone had already downloaded). `objects` and `bytes` above include what it brought. Omitted when no bundle was used. |

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

A clone that accepted a large-object promisor (`--blobs auto`):

```json
{
  "url": "https://git.example.com/project.git",
  "path": "project",
  "remote": "origin",
  "single_branch": false,
  "tag_fetch": "all",
  "branch": "main",
  "detached": false,
  "blobs": 5120,
  "partial": true,
  "filter_auto": true,
  "promisor_remotes": [
    {
      "name": "lop",
      "url": "https://lop.example.com/project.git",
      "accepted": true,
      "filter": "blob:limit=5k",
      "rejected": null
    }
  ],
  "objects": 3,
  "bytes": 225,
  "branches": 1,
  "tags": 0,
  "files": 2,
  "object_types": { "commits": 1, "trees": 1, "blobs": 1, "tags": 0 },
  "worktree_bytes": 20006,
  "seconds": 0.13,
  "phases": [],
  "bare": false
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

When the server advertised promisor remotes, a bullet lists them, and one more names each stored field:

```text
- **promisor remotes**: `lop` (accepted)
- **stored**: `partialCloneFilter` for `lop`
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Clone succeeded (including empty remote with no checkout). |
| `1` | Destination not empty, checkout failure (the repository is kept: `kind` `clone_checkout_failed`, see [When the checkout fails](#when-the-checkout-fails)), or other error. Missing or refused credentials are `kind` `auth_required` / `auth_rejected` with a `hint` in `--json` and on the human `hint:` line. HTTPS credentials come from your credential helper (`grit auth <url> --username <user> --token <token>` saves one for any host), then an askpass program (`GIT_ASKPASS`, `core.askPass`, `SSH_ASKPASS`); see [Credentials](../agents/#credentials-https). |
| `2` | Invalid flags ([`usage`](../exit-codes/)). |
| `128` | Could not reach or read the source: missing path, DNS or connection failure, HTTP error status ([`remote_unreadable`](../exit-codes/)); a `404` adds a hint to check the URL. |
| `130` | Interrupted with Ctrl-C, `SIGTERM`, or `SIGHUP` ([`interrupted`](../exit-codes/)). The error is `operation cancelled` (`{"error":"operation cancelled"}` with `--json`). |

See [Exit codes](../exit-codes/).

### When the checkout fails

A failure while downloading or setting up the repository removes the destination again. Once every object and ref is in place and `HEAD` is set, a failure while writing the work tree keeps the repository, as `git clone` does, so the checkout can be finished without downloading everything again. The usual cause is a partial clone whose missing blobs cannot be fetched: a large-object promisor that is unreachable or does not have them yet, or lazy fetch turned off. The error says the clone was kept, the `hint` says how to finish, and `--json` reports `kind` `clone_checkout_failed` with a `clone_checkout` object (exit `1`):

```console
$ GRIT_NO_LAZY_FETCH=1 grit clone --blobs none file:///srv/project.git project
Cloning into 'project' ...
error: the clone into 'project' succeeded, but its checkout failed: promised object not available locally: ce013625030ba8dba906f756967f9e9ca394464a
hint: the repository was kept; run `grit reset --hard` in 'project' to finish the checkout once its promisor remotes can supply the missing objects
$ grit -C project reset --hard
Reset master to 284c599 (hard)
```

| Field | Type | Description |
| --- | --- | --- |
| `clone_checkout.path` | string | The clone's directory, where the repository was kept. |
| `clone_checkout.commit` | string | The commit `HEAD` points at, whose files the checkout was writing. |
| `clone_checkout.missing_object` | string or omitted | The first promised object that could not be fetched, when the checkout failed for lack of one. |
| `clone_checkout.failed_remotes` | array | Each promisor remote that failed to supply the missing objects, in the order they were asked: `remote` (name) and `error` (its first error). Empty when no remote was asked, for example with lazy fetch off. |

A clone interrupted during the checkout is still removed (see below).

### Interrupting a clone

Ctrl-C, `SIGTERM`, and `SIGHUP` stop a clone at any point: while it downloads, copies objects or checks out files. The first signal asks the clone to stop; grit then rolls back the checkout, removes the destination directory it created (or empties one that already existed empty) and exits `130`. Nothing is left in the way, so the same `grit clone` command can simply be run again, and a bundle download carries on where it stopped. A duplicate signal that arrives with the first (`timeout -s INT` or `timeout -s TERM` signals both the process and its group) is ignored; sending the same signal again later ends grit at once, without the cleanup. Only an uncatchable `SIGKILL` (or that second signal after the duplicate window) skips rollback. In a Windows console Ctrl-C and Ctrl-Break behave the same way. Closing the console window still gives grit no chance to clean up, so delete the leftover directory by hand before you clone again.

```console
$ grit clone ../git git
Cloning into 'git' ...
^C
error: operation cancelled
$ echo $?
130
$ ls git
ls: cannot access 'git': No such file or directory
$ grit clone ../git git
Cloning into 'git' ...
✓ Cloned into git on master in 602ms

  time      ·█·············▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓  602ms
            █ copying 3ms  · other 194ms  ▓ checkout 387ms
  data      311 MiB received
  objects   422,423
  refs      1 branch · 1,012 tags
  files     4,856 files · 48.0 MiB in the work tree
```


## See also

[grit init](../init/), [grit remote](../remote/), [grit fetch](../fetch/), [grit auth](../auth/)