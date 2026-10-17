---
title: Objects
summary: ObjectId, ObjectKind, and reading or writing blobs, trees, and commits through Odb.
---

Git stores four object kinds grit-lib exposes as `ObjectKind`. Every object is named by an `ObjectId` (SHA-1 by default). Prefer `Repository::objects` (`Objects`) for read/write/contains; it delegates to the pluggable `Odb` on the same `Repository`.

## Writing

`Objects::write` (or `Odb::write`) takes a kind and payload bytes, stores a loose object under `objects/`, and returns the id. Tree and commit bodies must already be in Git’s text format; use `parse_tree` and `parse_commit` when reading them back.

`Odb::hash` (or `HashAlgo::hash_object`) computes an id without writing—useful for dry runs and tests. The algorithm follows the repository’s configured object format (SHA-1 or SHA-256).

## Reading

`Odb::read` returns an `Object` with `kind` and uncompressed `data`. Packed and loose objects share the same API.

When you only need type and size (for example listing objects without loading blob bodies), use `Odb::read_info` (see `read_info` on `Odb`). It returns `ObjectInfo` and avoids inflating full payloads for loose objects and non-delta pack entries; delta chains are resolved from headers and delta size varints only.

## Pack read caching

`Odb` owns a repository-scoped `PackStore`: pack directory listings, parsed `.idx` files, pack bytes, MIDX layers, and the delta-base LRU. Cloned `Odb` handles share the same store; alternate object directories get separate stores on the parent `Odb`.

After repack, garbage collection, or installing a pack with `install_pack_bytes`, call `Odb::invalidate_packs` so the next read rescans `objects/pack/`. If another `Odb` in the same process still holds a stale listing, a lookup miss retriggers directory reprepare when the pack folder’s mtime changes.

For batch reads (`cat-file --batch`, `--batch-all-objects`), wrap the loop in `Odb::with_pack_read_context` so pack indexes, mmap-backed pack bytes, and the delta-base LRU stay on one thread-local context instead of reinstalling it per object. `Odb::read` detects an active matching context and skips nested setup.

When iterating a pack in offset order (unordered `--batch-all-objects`), prefer `read_object_from_pack_at_offset` with the entry offset from `PackIndex` so the read path does not repeat index lookup by OID.

## Pack deltas (encoding)

When building new pack deltas (as opposed to reusing on-disk zlib), grit-lib indexes the base blob with `DeltaIndex` and encodes targets with a rolling fingerprint matcher. Reuse one index across many targets in a pack-objects window; `encode` on that index accepts an optional `max_size` cap (zero means no limit). An empty target yields a valid delta (header varints only). `encode_delta` is a convenience wrapper for one-off base/target pairs.

## Example

This example initializes a repository, writes a blob, tree, and commit, verifies structure in memory, and prints the commit id:

<!-- include: grit-examples/src/bin/guide_objects.rs -->

Objects land on disk in standard loose format. System Git can read them:

```bash
cargo run --bin guide_objects /path/to/repo
git -C /path/to/repo fsck --strict
```

When run without arguments the binary uses a temporary repository (fine for `cargo run`, not for fsck demos).
