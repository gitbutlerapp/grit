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

Packs local objects reachable from maintenance roots (refs, reflogs, and the index) into a single new packfile (similar to `git repack -ad`), removes older pack files, and deletes loose objects that are already stored in the new pack. Unlike full [`grit gc`](./gc.md), **`grit repack` always runs** (it does not apply `gc.auto` skip heuristics).

Objects already stored in a pack keep their stored form: whole objects are copied byte for byte and deltas are copied with only their base reference rewritten, as long as the base is packed too and the chain stays within `pack.depth`. Only the remaining objects (loose objects, and deltas whose base is dropped) go through a delta search over `pack.window` neighbours, so repacking an already packed repository costs about as much as copying it. With `--force` (like `git repack -adf`) every delta is recomputed, which takes longer but can make a pack written by a weaker packer smaller. Both modes stream object payloads and the new pack instead of holding them in memory.

Unreachable objects that lived only in removed packs are written loose first so they are not dropped silently; when [`grit gc`](./gc.md) runs with `gc.pruneExpire`, recently written unreachable objects are kept for the grace window before loose pruning.

In a shallow clone (`grit clone --depth` or `git clone --depth`), the commits listed in `.git/shallow` (in the common Git directory, shared by linked worktrees) are history boundaries, as in Git: their trees are packed, but their parents are neither packed nor required to exist.

In a partial clone, maintenance works offline after local commits. Existing `.promisor` packs and all their sidecars stay in place. The new unmarked pack contains local objects only; promised historical blobs or trees may remain absent. Promisor packs are never loosened or folded into the local pack. Bitmap generation is skipped when promises prevent a complete object closure. `--dry-run` follows the same boundary without fetching. `pack_path` and `pack_bytes` describe the newly written local pack even when promisor packs remain alongside it.

Use this when a repository has accumulated many loose objects after local commits. Clones and fetches already install packs; repack is mainly for long-lived worktrees built entirely with `grit commit`.

## Benchmarking

Measured on 2026-10-26 using Linux x86_64, four CPUs, 15 GiB RAM, system Git 2.55.0, and Rust 1.99.0 with the `release` profile. The real Grit repository fixture has 11,273 commits and 124,971 objects after adding one local indexed-file commit. Its blobless copy leaves 54,098 blobs promised, with the origin unavailable. Neither fixture has a checkout.

Each cell shows **wall time in ms / peak RSS in KiB**, as medians of seven fresh repository copies. Wall time includes process launch; RSS comes from `/usr/bin/time -v`. Complete-repository baseline and fixed runs alternate to control measurement variation. Git runs `gc` or `repack -a -d`; Grit runs `gc --json` or `repack --json`.

| Repository / operation | System Git | Baseline main (`8a358bf6a`) | Fixed |
| --- | ---: | ---: | ---: |
| Complete / gc | 1,687.1 / 296,896 | 1,707.2 / 294,292 | 1,707.7 / 294,988 |
| Complete / repack | 1,535.9 / 295,424 | 1,539.7 / 293,856 | 1,536.1 / 294,168 |
| Blobless / gc | 448.6 / 34,936 | 1,161.5 / 173,688 (failed) | 1,895.2 / 174,608 |
| Blobless / repack | 310.5 / 35,036 | 1,174.5 / 173,592 (failed) | 1,673.3 / 173,788 |

Baseline blobless GC exits 128 and repack exits 1 while trying the unavailable remote; those measurements cover failed operations. Fixed runs finish offline. Promisor classification still costs more time and memory than Git on this metadata-heavy fixture.

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
| `objects_packed` | number or null | Objects in the new local pack, or the estimated count on dry run (`null` when none). |
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
