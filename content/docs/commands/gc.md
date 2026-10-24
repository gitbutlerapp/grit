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
grit gc repack [--bitmaps] [--dry-run]
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
3. **repack** — consolidate reachable and retained unreachable objects into pack files, removing redundant loose objects and merging multiple packs when present. **Skips** rebuilding when the repository already has a single pack and no loose objects (typical after clone), unless repack side effects are requested (for example `repack.writeBitmaps`). When repack runs, existing on-disk deltas are reused for pack-only repositories unless loose objects need new deltas. Writes a `.bitmap` when `repack.writeBitmaps` is true or the repository is bare. Explicit [`grit repack`](repack.md) does not use this skip (`auto_skip` stays false).
4. **prune** — delete unreachable loose objects older than `gc.pruneExpire` (default `2.weeks.ago`). The object graph is only walked when some loose object is old enough to delete, and one gc walks it at most once (shared with the repack step), so gc of an already packed repository does not read the object graph at all.
5. **commit-graph** — write `objects/info/commit-graph`. Like `git gc`, changed-path Bloom filters are kept when the existing commit-graph has them and are not computed from scratch otherwise; run `grit gc commit-graph` to add them.

Task subcommands run a single step and return the same JSON shape with one entry in `tasks`.

Progress lines go to **stderr**. The human summary lists each task, key counters, and elapsed milliseconds.

## Options

| Option | Description |
| --- | --- |
| `--dry-run` | Preview maintenance without writing refs or objects (full `grit gc`, or combine with a task subcommand). |
| `-q`, `--quiet` | Suppress the human task summary. |

### `grit gc repack`

| Option | Description |
| --- | --- |
| `--bitmaps` | Write a reachability `.bitmap` sidecar for the new pack. |
| `--dry-run` | Report how many objects would be packed; no pack files or refs are modified. |

### `grit gc commit-graph`, `grit gc midx`, `grit gc pack-refs`

| Option | Description |
| --- | --- |
| `--dry-run` | Report counters and paths that would be written; leave the repository unchanged. |

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

## JSON output

Schema: [v1](/schemas/gc/v1.json) (task subcommands also have [gc.repack](/schemas/gc.repack/v1.json), [gc.commit-graph](/schemas/gc.commit-graph/v1.json), [gc.midx](/schemas/gc.midx/v1.json), [gc.pack-refs](/schemas/gc.pack-refs/v1.json), and [gc.prune](/schemas/gc.prune/v1.json) schemas).

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `dry_run` | boolean | Whether this run only estimated work (full gc). |
| `tasks` | array | One object per maintenance task; each value includes `task` (`pack-refs`, `reflog-expire`, `repack`, `prune`, `commit-graph`, `midx`) and task-specific counters plus `duration_ms`. |

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
