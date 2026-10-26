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
```

## Description

Runs Git-compatible maintenance in-process (never shells out to the system `git` binary).

Bare **`grit gc`** runs, in order:

1. **pack-refs** — merge loose refs into `packed-refs` (or compact reftable).
2. **reflog-expire** — apply `gc.reflogExpire*` rules using the current time.
3. **repack** — consolidate reachable and retained unreachable objects into pack files, removing redundant loose objects and merging multiple packs when present. **Skips** rebuilding when Git would (`gc.auto` loose count and `gc.autoPackLimit`, typically a single pack with no loose objects after clone), unless repack side effects are requested (for example `repack.writeBitmaps`). When repack runs, objects already in packs are copied in their stored form (deltas included) and only loose objects go through delta search, as described for [`grit repack`](repack.md). Writes a `.bitmap` when `repack.writeBitmaps` is true or the repository is bare. Explicit [`grit repack`](repack.md) does not use this skip (`auto_skip` stays false).
4. **prune** — delete unreachable loose objects older than `gc.pruneExpire` (default `2.weeks.ago`). The object graph is only walked when some loose object is old enough to delete, and one gc walks it at most once (shared with the repack step), so gc of an already packed repository does not read the object graph at all.
5. **commit-graph** — write `objects/info/commit-graph`. Like `git gc`, changed-path Bloom filters are kept when the existing commit-graph has them and are not computed from scratch otherwise; run `grit gc commit-graph` to add them. In a **shallow** clone, with `info/grafts`, or with replace refs (unless `core.useReplaceRefs=false`), commit parents differ from what the commit objects record, so Git uses no commit-graph there: grit writes none, reports the task as `skipped`, and deletes any existing graph (which `git fsck` would reject).

Every step that walks history (repack, prune, bitmap) stops at the shallow boundaries in `.git/shallow`, so gc works in shallow clones and keeps everything down to the boundary commits.

Task subcommands run a single step and return the same JSON shape with one entry in `tasks`.

Progress lines go to **stderr**. The human summary lists each task, key counters, and elapsed milliseconds.

### Automatic maintenance

Like Git, grit keeps loose objects from piling up without you scheduling anything. After a successful **`grit commit`**, **`merge`**, **`pick`**, **`revert`**, **`rebase`**, **`history`** rewrite, **`stash`** (push), **`fetch`** or **`pull`** (none of them with `--dry-run` or `--check`), grit runs the same check as `git gc --auto`:

- **Too many packs:** more local packs without a `.keep` file than `gc.autoPackLimit` (default 50). Every pack is consolidated into one, as a full `grit gc` does.
- **Too many loose objects:** the loose-object count is estimated from one fan-out directory (`objects/17`, times 256) and compared with `gc.auto` (default 6700). The reachable loose objects are packed into one new pack and existing packs are left alone, so the cost depends on the loose objects rather than the repository size. Unreachable loose objects stay loose until `gc.pruneExpire` (default `2.weeks.ago`; `now` expires them at once) lets the prune step delete them, as with Git.

When either fires, `grit gc --auto` runs: pack-refs, reflog expiry, the repack above, unreachable-object pruning under `gc.pruneExpire`, and commit-graph. The check itself is two directory reads, so commands do not get slower in the common case.

| Config | Default | Effect |
| --- | --- | --- |
| `gc.auto` | `6700` | Loose-object threshold. `0` or a negative value turns automatic maintenance off entirely. |
| `gc.autoPackLimit` | `50` | Pack-count threshold. `0` or a negative value turns off only this trigger. |
| `gc.autoDetach` | `true` | Run in the background (`grit gc --auto --quiet` in a separate process) so the command returns at once. `false` runs it before the command exits. |
| `maintenance.auto` | `true` | `false` stops commands from starting automatic maintenance; `grit gc --auto` still works. |

Before doing any work, an automatic run takes the `gc.pid` lock that Git uses, so it never overlaps another grit or Git gc, and runs the `pre-auto-gc` hook; a non-zero hook exit skips the run. In human mode the triggering command prints one line on stderr when it starts background maintenance, or after foreground maintenance has run (nothing when the hook declines or another gc holds the lock); `--json` and `--markdown` output are unchanged.

On Windows, the background child requests job breakaway, in addition to a detached console and a separate process group. This lets job-based command runners finish waiting for the triggering command while maintenance continues. If the surrounding job denies breakaway or the child otherwise cannot start, grit falls back to foreground maintenance: human output warns about the failed spawn and reports completed packing without claiming a background run.

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

Partial clones can run maintenance offline, including after creating local commits. GC retains existing `.promisor` packs and their sidecars separately from the new local-object pack. Promised historical blobs and trees may remain absent; GC, pruning, and `--dry-run` do not fetch them. Missing objects outside the known promise boundary still cause an error. Bitmaps are omitted when promises prevent a complete object closure.

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

Schema: [v1](/schemas/gc/v1.json) (task subcommands also have [gc.repack](/schemas/gc.repack/v1.json), [gc.commit-graph](/schemas/gc.commit-graph/v1.json), [gc.midx](/schemas/gc.midx/v1.json), [gc.pack-refs](/schemas/gc.pack-refs/v1.json), and [gc.prune](/schemas/gc.prune/v1.json) schemas).

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `dry_run` | boolean | Whether this run only estimated work (full gc). |
| `auto` | object | Only with `--auto`. `status` is `not-needed`, `already-running` (with `pid` and `host` from `gc.pid`, each `null` when unreadable), `declined-by-hook` (with the hook's `exit_code`), or `ran`. For `ran`, `trigger` is `loose-objects` (with `estimate` and `limit`) or `packs` (with `count` and `limit`). |
| `tasks` | array | One object per maintenance task; each value includes `task` (`pack-refs`, `reflog-expire`, `repack`, `prune`, `commit-graph`, `midx`) and task-specific counters plus `duration_ms`. |
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

A skipped commit-graph task adds a `- **skipped:** shallow` (or `grafts`, `replace-refs`) bullet.


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
