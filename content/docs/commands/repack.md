---
title: grit repack
summary: Pack reachable objects and remove redundant packs.
group: Maintenance
order: 2
---

## Synopsis

```text
grit repack
grit repack --force
grit repack --dry-run
grit repack --quiet
```

## Description

Packs every object reachable from maintenance roots (refs, reflogs, and the index) into a single packfile (similar to `git repack -ad`), removes older pack files, and deletes loose objects that are already stored in the new pack. Unlike full [`grit gc`](./gc.md), **`grit repack` always runs** (it does not apply `gc.auto` skip heuristics).

Objects already stored in a pack keep their stored form: whole objects are copied byte for byte and deltas are copied with only their base reference rewritten, as long as the base is packed too and the chain stays within `pack.depth`. Only the remaining objects (loose objects, and deltas whose base is dropped) go through a delta search over `pack.window` neighbours, so repacking an already packed repository costs about as much as copying it. With `--force` (like `git repack -adf`) every delta is recomputed, which takes longer but can make a pack written by a weaker packer smaller. Both modes stream object payloads and the new pack instead of holding them in memory.

Unreachable objects that lived only in removed packs are written loose first so they are not dropped silently; when [`grit gc`](./gc.md) runs with `gc.pruneExpire`, recently written unreachable objects are kept for the grace window before loose pruning.

Use this when a repository has accumulated many loose objects after local commits. Clones and fetches already install packs; repack is mainly for long-lived worktrees built entirely with `grit commit`.

## Options

| Option | Description |
| --- | --- |
| `-f`, `--force` | Recompute every delta instead of reusing the ones already in packs (whole objects are still copied when they stay whole). |
| `--dry-run` | Report how many objects would be packed without writing. |
| `-q`, `--quiet` | Suppress the human summary (JSON and Markdown output are unchanged). |

## Examples

Repack a repository with many loose commits:

```console
$ grit repack
Packed 412 objects into .git/objects/pack/pack-….pack (0 redundant pack files removed, 380 loose objects pruned).
```

Recompress a pack received from a server with full delta search:

```console
$ grit repack --force
Packed 195008 objects into .git/objects/pack/pack-….pack (1 redundant pack files removed, 0 loose objects pruned).
```

Preview only:

```console
$ grit repack --dry-run
Would pack 412 objects (380 loose, 0 pack files now).
```

## JSON output

Schema: [v1](/schemas/repack/v1.json)

Pass `--json` for stable, scripting-friendly output:

| Field | Type | Meaning |
| ----- | ---- | ------- |
| `dry_run` | boolean | Whether this run only estimated work. |
| `loose_before` | number | Loose object files before repack. |
| `packs_before` | number | `.pack` / `.idx` pairs before repack. |
| `objects_packed` | number or null | Objects in the new pack (`null` on dry run). |
| `pack_bytes` | number or null | Size of the pack payload in bytes. |
| `pack_path` | string or null | Path to the installed `.pack` file. |
| `removed_pack_files` | number | Pack sidecar files removed. |
| `pruned_loose` | number | Loose files removed after packing. |

Example:

```json
{
  "dry_run": false,
  "loose_before": 380,
  "packs_before": 0,
  "objects_packed": 412,
  "pack_bytes": 88234,
  "pack_path": ".git/objects/pack/pack-abc123….pack",
  "removed_pack_files": 0,
  "pruned_loose": 380
}
```

## Markdown output

Pass `--markdown` for an agent-friendly summary with the same fields as JSON in bullet and table form.

Schema: [v1](/schemas/repack/v1.json)




```text
## Repack

- **dry_run:** true
- **loose_before:** 9
- **packs_before:** 0
- **objects_packed:** 9
- **removed_pack_files:** 0
- **pruned_loose:** 0
```

## Exit status

| Code | Meaning |
| ---: | --- |
| `0` | Success ([`success`](../exit-codes/)). |
| `1` | Command failed ([`failure`](../exit-codes/)). |
| `2` | Invalid flags or arguments ([`usage`](../exit-codes/)). |

See [Exit codes](../exit-codes/) for shared rules and structured `--json` `kind` values.


## See also

- [`grit gc`](gc.md) — repack plus unreachable loose-object prune
- [`grit fsck`](fsck.md) — verify integrity after maintenance
