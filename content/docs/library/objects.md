---
title: Objects
summary: ObjectId, ObjectKind, and reading or writing blobs, trees, and commits through Odb.
---

Git stores four object kinds grit-lib exposes as [`ObjectKind`](rustdoc:grit_lib::objects::ObjectKind). Every object is named by an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId) (SHA-1 by default). The [`Odb`](rustdoc:grit_lib::odb::Odb) on [`Repository`](rustdoc:grit_lib::repo::Repository) reads loose objects and packed storage transparently.

## Writing

[`Odb::write`](rustdoc:grit_lib::odb::Odb) takes a kind and payload bytes, stores a loose object under `objects/`, and returns the id. Tree and commit bodies must already be in Git’s text format; use [`parse_tree`](rustdoc:grit_lib::objects::parse_tree) and [`parse_commit`](rustdoc:grit_lib::objects::parse_commit) when reading them back.

[`Odb::hash`](rustdoc:grit_lib::odb::Odb) (or [`HashAlgo::hash_object`](rustdoc:grit_lib::objects::HashAlgo)) computes an id without writing—useful for dry runs and tests. The algorithm follows the repository’s configured object format (SHA-1 or SHA-256).

## Reading

[`Odb::read`](rustdoc:grit_lib::odb::Odb) returns an [`Object`](rustdoc:grit_lib::objects::Object) with `kind` and uncompressed `data`. Packed and loose objects share the same API.

When you only need type and size (for example listing objects without loading blob bodies), use [`Odb::read_info`](rustdoc:grit_lib::odb::Odb) (see `read_info` on [`Odb`](rustdoc:grit_lib::odb::Odb)). It returns [`ObjectInfo`](rustdoc:grit_lib::objects::ObjectInfo) and avoids inflating full payloads for loose objects and non-delta pack entries; delta chains are resolved from headers and delta size varints only.

## Pack read caching

[`Odb`](rustdoc:grit_lib::odb::Odb) owns a repository-scoped [`PackStore`](rustdoc:grit_lib::pack_store::PackStore): pack directory listings, parsed `.idx` files, pack bytes, MIDX layers, and the delta-base LRU. Cloned [`Odb`](rustdoc:grit_lib::odb::Odb) handles share the same store; alternate object directories get separate stores on the parent [`Odb`](rustdoc:grit_lib::odb::Odb).

After repack, garbage collection, or installing a pack with [`install_pack_bytes`](rustdoc:grit_lib::index_pack::install_pack_bytes), call [`Odb::invalidate_packs`](rustdoc:grit_lib::odb::Odb) so the next read rescans `objects/pack/`. If another [`Odb`](rustdoc:grit_lib::odb::Odb) in the same process still holds a stale listing, a lookup miss retriggers directory reprepare when the pack folder’s mtime changes.

For batch reads (`cat-file --batch`, `--batch-all-objects`), wrap the loop in [`Odb::with_pack_read_context`](rustdoc:grit_lib::odb::Odb::with_pack_read_context) so pack indexes, mmap-backed pack bytes, and the delta-base LRU stay on one thread-local context instead of reinstalling it per object. [`Odb::read`](rustdoc:grit_lib::odb::Odb::read) detects an active matching context and skips nested setup.

When iterating a pack in offset order (unordered `--batch-all-objects`), prefer [`read_object_from_pack_at_offset`](rustdoc:grit_lib::pack::read_object_from_pack_at_offset) with the entry offset from [`PackIndex`](rustdoc:grit_lib::pack::PackIndex) so the read path does not repeat index lookup by OID.

## Example

This example initializes a repository, writes a blob, tree, and commit, verifies structure in memory, and prints the commit id:

<!-- include: grit-examples/src/bin/guide_objects.rs -->

Objects land on disk in standard loose format. System Git can read them:

```bash
cargo run --bin guide_objects /path/to/repo
git -C /path/to/repo fsck --strict
```

When run without arguments the binary uses a temporary repository (fine for `cargo run`, not for fsck demos).
