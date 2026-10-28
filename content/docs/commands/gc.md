---
title: grit gc
summary: Run repository maintenance — pack refs, expire reflogs, repack, prune, and refresh commit-graph.
group: Maintenance
order: 3
---

## Synopsis

```text
grit gc
grit gc --dry-run
grit gc --quiet
grit gc --auto [--dry-run]
grit gc repack [--bitmaps] [--force] [--dry-run]
grit gc commit-graph [--dry-run]
grit gc midx [--dry-run]
grit gc pack-refs [--dry-run]
grit gc prune [--older-than DATE] [--dry-run]
grit gc offload --to REMOTE [--blobs none|SIZE] [--lop-dir PATH] [--configure-lop] [--dry-run]
```

## Description

Runs Git-compatible maintenance in-process (never shells out to the system `git` binary).

Bare **`grit gc`** runs, in order:

1. **pack-refs** — merge loose refs into `packed-refs` (or compact reftable), unless `gc.packRefs` is `false`. Empty ref subdirectories are removed; `refs/heads`, `refs/tags`, and `refs/remotes` stay as directories even when empty, like Git.
2. **reflog-expire** — apply `gc.reflogExpire*` rules using the current time. Entries older than `gc.reflogExpireUnreachable` are checked against the commits reachable from the ref, as described for [`grit reflog expire`](reflog.md); each commit is read once per run however many reflogs need it, and each ref's walk only marks commits already read, so this phase stays fast on a large repository with month-old reflogs and many branches.
3. **repack** — consolidate reachable and retained unreachable objects into pack files, removing redundant loose objects and merging multiple packs when present. In a clone with `objects/info/alternates`, only objects stored in this repository are packed (Git's `repack -l` / `pack-objects --local`); the rest keep being read from the alternate. **Skips** rebuilding when Git would (`gc.auto` loose count and `gc.autoPackLimit`, typically a single pack with no loose objects after clone), unless repack side effects are requested (for example `repack.writeBitmaps` or `pack.writeBitmaps`). When repack runs, objects already in packs are copied in their stored form (deltas included) and only loose objects go through delta search, as described for [`grit repack`](repack.md). Writes a `.bitmap` when the winning `repack.writeBitmaps` or `pack.writeBitmaps` entry is true, or when neither is set and the repository is bare. Explicit [`grit repack`](repack.md) does not use this skip (`auto_skip` stays false).
4. **prune** — delete unreachable loose objects older than `gc.pruneExpire` (default `2.weeks.ago`). The object graph is only walked when some loose object is old enough to delete, and one gc walks it at most once (shared with the repack step), so gc of an already packed repository does not read the object graph at all.
5. **commit-graph** — write `objects/info/commit-graph` when `gc.writeCommitGraph` is not `false` and `core.commitGraph` is not `false`. Like `git gc`, changed-path Bloom filters are kept when the existing commit-graph has them and are not computed from scratch otherwise; run `grit gc commit-graph` to add them (that subcommand ignores `gc.writeCommitGraph`). In a **shallow** clone, with `info/grafts`, or with replace refs (unless `core.useReplaceRefs=false`), commit parents differ from what the commit objects record, so Git uses no commit-graph there: grit writes none, reports the task as `skipped`, and deletes any existing graph (which `git fsck` would reject).

Every step that walks history (repack, prune, bitmap) stops at the shallow boundaries in `.git/shallow`, so gc works in shallow clones and keeps everything down to the boundary commits.

Task subcommands run a single step and return the same JSON shape with one entry in `tasks`.

Progress lines go to **stderr**. The human summary lists each task, key counters, and elapsed milliseconds.

### Automatic maintenance

Like Git, grit keeps loose objects from piling up without you scheduling anything. After a successful **`grit commit`**, **`merge`**, **`pick`**, **`revert`**, **`rebase`**, **`history`** rewrite, **`stash`** (push), **`fetch`** or **`pull`** (none of them with `--dry-run` or `--check`), grit runs the same check as `git gc --auto`:

- **Too many packs:** more local packs without a `.keep` file than `gc.autoPackLimit` (default 50). Every pack is consolidated into one, as a full `grit gc` does.
- **Too many loose objects:** the loose-object count is estimated from one fan-out directory (`objects/17`, times 256) and compared with `gc.auto` (default 6700). The reachable loose objects are packed into one new pack and existing packs are left alone, so the cost depends on the loose objects rather than the repository size. The new pack is delta-compressed with `pack.window` and `pack.depth` like [`grit repack`](repack.md), pairing each file's versions by path, so successive versions of a large binary cost one copy plus small deltas; it never deltas against objects in older packs. Unreachable loose objects stay loose until `gc.pruneExpire` (default `2.weeks.ago`; `now` expires them at once) lets the prune step delete them, as with Git.

When either fires, `grit gc --auto` runs: pack-refs, reflog expiry, the repack above, unreachable-object pruning under `gc.pruneExpire`, and commit-graph. The check itself is two directory reads, so commands do not get slower in the common case.

| Config | Default | Effect |
| --- | --- | --- |
| `gc.auto` | `6700` | Loose-object threshold. `0` or a negative value turns automatic maintenance off entirely. |
| `gc.autoPackLimit` | `50` | Pack-count threshold. `0` or a negative value turns off only this trigger. |
| `gc.autoDetach` | `true` | Run in the background (`grit gc --auto --quiet` in a separate process) so the command returns at once. `false` runs it before the command exits. |
| `maintenance.auto` | `true` | `false` stops commands from starting automatic maintenance; `grit gc --auto` still works. |

Full **`grit gc`** (and `grit gc --auto` when it runs every step) also honors:

| Config | Default | Effect |
| --- | --- | --- |
| `gc.writeCommitGraph` | `true` | When `false`, skip the commit-graph step; any existing graph is kept. Does not affect `grit gc commit-graph`. Also skipped when `core.commitGraph=false`. |
| `gc.packRefs` | `true` | When `false`, skip pack-refs during gc. `notbare` packs only in non-bare repositories (bare repos keep loose refs). Does not affect `grit gc pack-refs`. |
| `repack.writeBitmaps` | unset (bare: true) | When true, gc repack writes a reachability `.bitmap` even if it must rewrite a single pack. |
| `pack.writeBitmaps` | — | Deprecated Git synonym of `repack.writeBitmaps`; when both keys are set, the last entry in config load order wins. |

Before doing any work, an automatic run takes the `gc.pid` lock that Git uses, so it never overlaps another grit or Git gc, and runs the `pre-auto-gc` hook; a non-zero hook exit skips the run. In human mode the triggering command prints one line on stderr when it starts background maintenance, or after foreground maintenance has run (nothing when the hook declines or another gc holds the lock); `--json` and `--markdown` output are unchanged.

On Windows, the background child requests job breakaway, in addition to a detached console and a separate process group. This lets job-based command runners finish waiting for the triggering command while maintenance continues. The child also does not inherit grit's stdin, stdout or stderr, so a caller that reads the command's output until it closes (PowerShell, `cmd /c`, a captured spawn) gets it as soon as the command exits, not when packing finishes. If the surrounding job denies breakaway or the child otherwise cannot start, grit falls back to foreground maintenance: human output warns about the failed spawn and reports completed packing without claiming a background run.

A plain `grit gc` always runs and also takes the `gc.pid` lock: it fails with an error when another gc is running.

### Commit benchmark

An 8,000-file first commit across 160 directories, with default `gc.auto=6700`:

| Command | Median commit wall time | Peak RSS |
| --- | --- | --- |
| System Git 2.55.0 | 170.56 ms | 9,996 KiB |
| grit before Windows job breakaway (`a1f4e2168`) | 62.49 ms | 19,904 KiB |
| grit with Windows job breakaway | 62.17 ms | 19,744 KiB |

Measured on an Ubuntu 24.04 x86-64 VM with four AMD EPYC vCPUs, using `dist` builds and `/usr/bin/time -v`. Five fresh repositories per command were measured in alternating order. Each tool staged its own files before timing; filesystem writes were flushed before the commit, with warm caches. RSS is the highest per-run peak across the five runs and excludes detached maintenance. Each background run finished before the next measurement.

Every commit returned with 8,162 loose objects and no packed objects; background maintenance subsequently completed and `git fsck --strict --no-dangling` passed. Linux already detached on the baseline, so these measurements check the unchanged Unix path, not Windows job-object behavior. Windows jobs that prohibit breakaway still require the foreground fallback described above.

Partial clones can run maintenance offline, including after creating local commits. GC retains existing `.promisor` packs and their sidecars separately from the new local-object pack. Ordinary packs that hold promised objects unavailable in the retained promisor packs also stay in place, preserving offline access to content already present locally. Promised historical blobs and trees may remain absent; GC, pruning, and `--dry-run` do not fetch them. Missing objects outside the known promise boundary still cause an error. Bitmaps are omitted when promises prevent a complete object closure.

## Options

| Option | Description |
| --- | --- |
| `--dry-run` | Preview maintenance without writing refs or objects (full `grit gc`, or combine with a task subcommand). |
| `--auto` | Run only when `gc.auto` or `gc.autoPackLimit` is exceeded (see [Automatic maintenance](#automatic-maintenance)). With `--dry-run`, report the decision and the planned work. Cannot be combined with a task subcommand. |
| `-q`, `--quiet` | Suppress the human task summary. |

### `grit gc repack`

| Option | Description |
| --- | --- |
| `--bitmaps` | Write a reachability `.bitmap` sidecar for the new pack. |
| `-f`, `--force` | Recompute every delta instead of reusing the ones already in packs (like `git repack -f`). |
| `--dry-run` | Report how many objects would be packed; no pack files or refs are modified. |

### `grit gc commit-graph`, `grit gc midx`, `grit gc pack-refs`

| Option | Description |
| --- | --- |
| `--dry-run` | Report counters and paths that would be written; leave the repository unchanged. |

`grit gc midx` writes multi-pack-index format version 1, which every Git with multi-pack-index support reads (Git 2.43 included). Set `midx.version = 2` in config to write version 2 instead; only newer Git releases read it. Any other `midx.version` value is an error, even with `--dry-run`, and nothing is written.

### `grit gc prune`

| Option | Description |
| --- | --- |
| `--older-than DATE` | Git approxidate cutoff; default is `gc.pruneExpire` from config. |
| `--dry-run` | Report unreachable loose objects that would be deleted without removing them. |

Like `git prune`, `grit gc prune` returns immediately when there is no loose object older than the cutoff: it only checks that the objects named by refs, reflogs and the index exist instead of walking the object graph.

### `grit gc offload`

Moves large blobs out of the repository to a *large-object promisor* (LOP): a second repository that holds only the big blobs. The repository you run it in becomes a partial repository of the LOP. Its `remote.<REMOTE>.promisor` is set, and its remaining objects move into one `.promisor` pack. Clients that accept the LOP through the `promisor-remote` capability get packs without the offloaded blobs and fetch those from the LOP themselves (see [grit upload-pack](upload-pack.md#serving-a-repository-whose-large-blobs-live-elsewhere)). This is the layout Git's own large-object-promisor setup uses, built there with `git repack -a -d --filter=blob:limit=<n> --filter-to=<lop>`.

| Option | Description |
| --- | --- |
| `--to REMOTE` | The promisor remote the blobs go to. `remote.<REMOTE>.url` must be set. |
| `--blobs none\|SIZE` | Which blobs move: `none` moves every blob, a size such as `512k` or `1m` moves blobs of at least that many bytes (`k`, `m`, `g` are KiB, MiB, GiB, as for [`grit clone --blobs`](clone.md)). Defaults to `remote.<REMOTE>.partialCloneFilter`; one of the two is required. A given value is also stored as `partialCloneFilter` when that is unset, so `promisor.sendFields = partialCloneFilter` passes it on to clients. |
| `--lop-dir PATH` | The LOP's git directory on this machine, for when `remote.<REMOTE>.url` is the URL clients use (such as `https://…`) rather than a local path. |
| `--configure-lop` | Also set `uploadpack.allowFilter` and `uploadpack.allowAnySHA1InWant` in the LOP, which it needs to answer clients' lazy fetches. |
| `--dry-run` | Report what would move; write nothing. |

Nothing is removed from the repository before the LOP holds it. The blobs go into one new LOP pack, whole, with a `.keep` marker written before the pack appears: nothing in the LOP refers to them, so this protects them from `gc` there. A blob the LOP already holds in a `.keep` pack of its own is not copied again. A loose or unkept copy there is copied anyway, since the LOP's next `gc` may prune it, and so is a copy the LOP only reaches through its alternates. Every blob is checked to be in a `.keep` pack of the LOP, and the new pack is flushed to disk. Only then are the config set and the repository's own packs and loose objects rewritten without the blobs. Unreachable objects are kept, and a pack pushed during the run is left alone. Blobs that a checkout needs (a worktree's index or `HEAD`) stay, as do blobs in `.keep` packs. `promisor.advertise` and `extensions.partialClone` are left for you to set. The run takes the `gc.pid` lock like `grit gc`.

Besides `remote.<REMOTE>.promisor`, offload sets `repack.writeBitmaps = false` in the repository, and the repository cannot use reachability bitmaps afterwards. A bitmap, for one pack or a multi-pack-index, needs every object its commits reach in the packs it covers, and the offloaded blobs are in none of them. Git's repack keeps promisor and other objects in separate packs, so once a push adds a commit outside the promisor pack, Git fails with `Packfile doesn't have full closure`: `git gc` exits 128 on a bare repository, where bitmaps are on by default, and `git repack -a -d -b --write-midx` fails as well (Git 2.50 and 2.56). `grit gc` and `grit gc repack --bitmaps` leave the bitmap out instead, so the setting is there for Git. Git's own `git repack --filter-to` setup fails the same way on a bare repository unless bitmaps are off.

Run it again whenever new large blobs have been pushed. It moves only those. Each run also folds objects pushed since the last one back into the single `.promisor` pack, even when no blob moves, so later fetches can tell from the pack alone that an offloaded blob is promised. It does nothing when there is nothing to move or fold. `grit gc`, `git gc` and `git fsck` keep the offloaded blobs promised and never fetch them back, and pushes to the repository (through `grit receive-pack` or Git's) may name offloaded blobs in their trees.

#### Setting up a large-object promisor

```console
$ grit init --bare /srv/lop.git
$ cd /srv/main.git
$ grit remote add lop /srv/lop.git
$ grit config promisor.advertise true
$ grit config promisor.sendFields partialCloneFilter
$ grit config uploadpack.allowFilter true
$ grit gc offload --to lop --blobs 1m --configure-lop
offload: moved 12 blobs (1.4 GiB) to lop; main packs 1.5 GiB -> 92 MiB (3104 ms).
  set remote.lop.promisor = true (main)
  set remote.lop.partialCloneFilter = blob:limit=1048576 (main)
  set repack.writeBitmaps = false (main)
  set uploadpack.allowFilter = true (lop)
  set uploadpack.allowAnySHA1InWant = true (lop)
```

Clients then clone with the LOP accepted, for example `git clone -c remote.lop.url=… -c remote.lop.promisor=true -c promisor.acceptFromServer=All --filter=blob:limit=1m <url>`, or `grit clone --blobs auto` with the same `--config` values. Git 2.49 and newer understand the `promisor-remote` capability; before 2.54 the client must pass `--filter` itself. A client that accepts nothing is refused with a hint, unless the server lazily fetches the blobs for it (`uploadpack.lazyFetch = true`, or `GIT_NO_LAZY_FETCH=0` for `grit upload-pack` / `grit serve`). When clients reach the LOP at a different URL than the server, give `remote.lop.url` that URL and pass `--lop-dir /srv/lop.git` to offload.

## Examples

```console
$ grit gc
reflog-expire: removed 0 entries (12 ms).
repack: packed 412 objects into .git/objects/pack/pack-….pack (842 ms).
commit-graph: wrote 87 commits to .git/objects/info/commit-graph (120 ms).
```

Dry run (reflog expiry and repack estimate only; JSON includes the same counters):

```console
$ grit gc --dry-run
Dry run: no changes written.
reflog-expire: removed 0 entries (11 ms).
repack: would pack 412 objects (380 loose, 0 pack files now) (268 ms).
```

Single task:

```console
$ grit gc repack --bitmaps
repack: packed 412 objects into .git/objects/pack/pack-….pack (800 ms).
```

Automatic maintenance, below and above the threshold:

```console
$ grit gc --auto
auto: nothing to do (below gc.auto and gc.autoPackLimit).
$ grit gc --auto
auto: ran for about 8192 loose objects (gc.auto = 6700).
pack-refs: packed 1 refs (1 loose removed) → .git/packed-refs (0 ms).
reflog-expire: removed 0 entries (3 ms).
repack: packed 9633 objects into .git/objects/pack/pack-….pack (469 ms).
prune: removed 0 unreachable loose objects (0 kept, 0 ms).
commit-graph: wrote 2401 commits to .git/objects/info/commit-graph (25 ms).
```

With `--json`, the decision is in `auto` (abbreviated):

```json
{
  "dry_run": false,
  "auto": {
    "status": "ran",
    "trigger": "loose-objects",
    "estimate": 8192,
    "limit": 6700
  },
  "tasks": [
    {
      "task": "repack",
      "duration_ms": 469,
      "loose_before": 9633,
      "packs_before": 0,
      "objects_packed": 9633,
      "pack_bytes": 2675908,
      "pack_path": ".git/objects/pack/pack-6ac7a5ee….pack",
      "removed_pack_files": 0,
      "pruned_loose": 9633,
      "unreachable_loose_pruned": 0
    }
  ]
}
```

In a shallow clone:

```console
$ grit gc commit-graph
commit-graph: skipped, parents are rewritten (shallow); any existing graph removed (0 ms).
```

## JSON output

Schema: [v1](/schemas/gc/v1.json) (task subcommands also have [gc.repack](/schemas/gc.repack/v1.json), [gc.commit-graph](/schemas/gc.commit-graph/v1.json), [gc.midx](/schemas/gc.midx/v1.json), [gc.pack-refs](/schemas/gc.pack-refs/v1.json), [gc.prune](/schemas/gc.prune/v1.json), and [gc.offload](/schemas/gc.offload/v1.json) schemas).

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `dry_run` | boolean | Whether this run only estimated work (full gc). |
| `auto` | object | Only with `--auto`. `status` is `not-needed`, `already-running` (with `pid` and `host` from `gc.pid`, each `null` when unreadable), `declined-by-hook` (with the hook's `exit_code`), or `ran`. For `ran`, `trigger` is `loose-objects` (with `estimate` and `limit`) or `packs` (with `count` and `limit`). |
| `tasks` | array | One object per maintenance task; each value includes `task` (`pack-refs`, `reflog-expire`, `repack`, `prune`, `commit-graph`, `midx`, `offload`) and task-specific counters plus `duration_ms`. |
| `tasks[].skipped` | string | `commit-graph` only, present when no graph was written: `shallow`, `grafts` or `replace-refs`. `commit_count` is then `0` and any existing graph has been deleted (dry run: would be). |

Example (abbreviated):

```json
{
  "dry_run": false,
  "tasks": [
    {
      "task": "reflog-expire",
      "duration_ms": 12,
      "entries_pruned": 0
    },
    {
      "task": "repack",
      "duration_ms": 842,
      "loose_before": 380,
      "packs_before": 0,
      "objects_packed": 412,
      "pack_bytes": 88234,
      "pack_path": ".git/objects/pack/pack-abc123….pack",
      "removed_pack_files": 0,
      "pruned_loose": 380,
      "unreachable_loose_pruned": 0
    },
    {
      "task": "commit-graph",
      "duration_ms": 120,
      "commit_count": 87,
      "path": ".git/objects/info/commit-graph",
      "removed_chain": false
    }
  ]
}
```

The `offload` task (from `grit gc offload`) has these fields:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `tasks[].remote` | string | The promisor remote the blobs moved to. |
| `tasks[].lop_git_dir` | string | The LOP's git directory. |
| `tasks[].filter` | string | The filter that chose the blobs, `blob:limit=<bytes>` or `blob:none`. |
| `tasks[].candidates` | number | Blobs selected that may move. |
| `tasks[].candidate_bytes` | number | Their uncompressed size. |
| `tasks[].copied_to_lop` | number | Blobs written into a new LOP pack. |
| `tasks[].already_in_lop` | number | Blobs the LOP already had in a `.keep` pack of its own. |
| `tasks[].dropped_from_main` | number | Blobs no longer stored here afterwards (`0` on a dry run). |
| `tasks[].skipped_checked_out` | number | Selected blobs kept because a worktree's index or `HEAD` needs them. |
| `tasks[].skipped_kept_pack` | number | Selected blobs kept because they are in a `.keep` pack. |
| `tasks[].lop_pack` | string or null | The pack added to the LOP. |
| `tasks[].main_pack` | string or null | The `.promisor` pack the rest of the repository now lives in; null when nothing had to be rewritten. |
| `tasks[].main_bytes_before` | number | Size of this repository's packs and loose objects before the run. |
| `tasks[].main_bytes_after` | number | The same afterwards. |
| `tasks[].config_changes` | array | Each `{repo, key, value}` set, `repo` being `main` or `lop`. |
| `tasks[].lop_ready` | object | `allow_filter` and `allow_any_sha1_in_want`: the LOP's upload-pack settings afterwards. |
| `tasks[].advertised` | boolean | `promisor.advertise` is on, so clients are told about the LOP. |

```json
{
  "dry_run": false,
  "tasks": [
    {
      "task": "offload",
      "duration_ms": 95,
      "remote": "lop",
      "lop_git_dir": "/srv/lop.git",
      "filter": "blob:limit=102400",
      "candidates": 2,
      "candidate_bytes": 500000,
      "copied_to_lop": 2,
      "already_in_lop": 0,
      "dropped_from_main": 2,
      "skipped_checked_out": 0,
      "skipped_kept_pack": 0,
      "lop_pack": "/srv/lop.git/objects/pack/pack-bc97733c….pack",
      "main_pack": "/srv/main.git/objects/pack/pack-32d0e589….pack",
      "main_bytes_before": 500813,
      "main_bytes_after": 1690,
      "config_changes": [
        { "repo": "main", "key": "remote.lop.promisor", "value": "true" },
        { "repo": "main", "key": "remote.lop.partialCloneFilter", "value": "blob:limit=102400" },
        { "repo": "main", "key": "repack.writeBitmaps", "value": "false" }
      ],
      "lop_ready": { "allow_filter": true, "allow_any_sha1_in_want": true },
      "advertised": true
    }
  ]
}
```

`grit gc commit-graph --json` in a shallow clone:

```json
{
  "dry_run": false,
  "tasks": [
    {
      "task": "commit-graph",
      "duration_ms": 0,
      "commit_count": 0,
      "path": ".git/objects/info/commit-graph",
      "removed_chain": false,
      "skipped": "shallow"
    }
  ]
}
```

## Markdown output

Pass `--markdown` for an agent-friendly report with the same fields as JSON as labelled bullets per task.

Schema: [v1](/schemas/gc/v1.json)

Example (abbreviated):

```markdown
## Garbage collection

- **dry_run:** false

### Tasks

#### reflog-expire

- **duration_ms:** 12
- **entries_pruned:** 0

#### repack

- **duration_ms:** 842
- **loose_before:** 380
- **packs_before:** 0
- **objects_packed:** 412
- **pack_bytes:** 88234
- **pack_path:** `.git/objects/pack/pack-abc123….pack`
- **removed_pack_files:** 0
- **pruned_loose:** 380
- **unreachable_loose_pruned:** 0

#### commit-graph

- **duration_ms:** 120
- **commit_count:** 87
- **path:** `.git/objects/info/commit-graph`
- **removed_chain:** false
```

A skipped commit-graph task adds a `- **skipped:** shallow` (or `grafts`, `replace-refs`) bullet. An `offload` task lists its JSON fields as bullets, one `config_change` bullet per value set, and `lop_allow_filter` / `lop_allow_any_sha1_in_want` for `lop_ready`.


## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

- [`grit repack`](repack.md)
- [`grit fsck`](fsck.md)
- [`grit reflog expire`](reflog.md)
