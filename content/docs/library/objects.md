---
title: Objects
summary: ObjectId, ObjectKind, and reading or writing blobs, trees, and commits through Odb.
---

Git stores four object kinds grit-lib exposes as [`ObjectKind`](rustdoc:grit_lib::objects::ObjectKind). Every object is named by an [`ObjectId`](rustdoc:grit_lib::objects::ObjectId) (SHA-1 by default). Prefer [`Repository::objects`](rustdoc:grit_lib::repo::Repository) ([`Objects`](rustdoc:grit_lib::objects::handle::Objects)) for read/write/contains; it delegates to the pluggable [`Odb`](rustdoc:grit_lib::objects::odb::Odb) on the same [`Repository`](rustdoc:grit_lib::repo::Repository).

## Writing

[`Objects::write`](rustdoc:grit_lib::objects::handle::Objects) (or [`Odb::write`](rustdoc:grit_lib::objects::odb::Odb)) takes a kind and payload bytes, stores a loose object under `objects/`, and returns the id. Tree and commit bodies must already be in Git’s text format; use [`parse_tree`](rustdoc:grit_lib::objects::parse_tree) and [`parse_commit`](rustdoc:grit_lib::objects::parse_commit) when reading them back.

[`Odb::hash`](rustdoc:grit_lib::objects::odb::Odb) (or [`HashAlgo::hash_object`](rustdoc:grit_lib::objects::HashAlgo)) computes an id without writing—useful for dry runs and tests. The algorithm follows the repository’s configured object format (SHA-1 or SHA-256).

## Reading

[`Odb::read`](rustdoc:grit_lib::objects::odb::Odb) returns an [`Object`](rustdoc:grit_lib::objects::Object) with `kind` and uncompressed `data`. Packed and loose objects share the same API.

For hot loops (tree walks, batch cat-file), [`Odb::read_into`](rustdoc:grit_lib::objects::odb::Odb::read_into) inflates into a caller-owned [`Vec`](https://doc.rust-lang.org/std/vec/struct.Vec.html) and returns the [`ObjectKind`](rustdoc:grit_lib::objects::ObjectKind). Payload bytes stay in the buffer; wrap them with [`ObjectRef::from_buf`](rustdoc:grit_lib::objects::ObjectRef::from_buf) when you need a borrowed view. The same method exists on [`ObjectStore`](rustdoc:grit_lib::objects::odb::store::ObjectStore) backends (default: copy from [`ObjectStore::read`](rustdoc:grit_lib::objects::odb::store::ObjectStore::read); native for loose files and packs).

When you only need type and size (for example listing objects without loading blob bodies), use [`Odb::read_info`](rustdoc:grit_lib::objects::odb::Odb) (see `read_info` on [`Odb`](rustdoc:grit_lib::objects::odb::Odb)). It returns [`ObjectInfo`](rustdoc:grit_lib::objects::ObjectInfo) and avoids inflating full payloads for loose objects and non-delta pack entries; delta chains are resolved from headers and delta size varints only.

## Tree iteration without copying names

[`parse_tree`](rustdoc:grit_lib::objects::parse_tree) allocates an owning [`TreeEntry`](rustdoc:grit_lib::objects::TreeEntry) for every name. For hot paths that only walk a tree once (object walks, pack enumeration, fsck), use [`TreeRefIter`](rustdoc:grit_lib::objects::TreeRefIter): it yields [`TreeEntryRef`](rustdoc:grit_lib::objects::TreeEntryRef) values that borrow name bytes from the tree blob. Pass the repository hash width with [`TreeRefIter::with_oid_len`](rustdoc:grit_lib::objects::TreeRefIter::with_oid_len), or call [`TreeRefIter::infer_oid_len`](rustdoc:grit_lib::objects::TreeRefIter::infer_oid_len) when the algorithm is not threaded through (same SHA-1-then-SHA-256 detection as [`parse_tree`](rustdoc:grit_lib::objects::parse_tree)). Call [`TreeEntryRef::to_owned`](rustdoc:grit_lib::objects::TreeEntryRef::to_owned) when you need a [`TreeEntry`](rustdoc:grit_lib::objects::TreeEntry). The compiled example below reads a tree with both APIs.

## Peeling tags

A ref tip can name an annotated tag, or a tag of a tag. `Odb::peel_tags` (on [`Odb`](rustdoc:grit_lib::objects::odb::Odb)) follows the chain and returns the id and kind of the first non-tag object; `Odb::peel_to_commit` returns `Some(commit)` when that object is a commit and `None` for a tree or blob. Only tag objects are read in full, and only their `object` header is parsed, so tags with non-UTF-8 messages peel fine; the final object's kind comes from `read_info`. Chains of any length are followed. A missing object, a tag without a valid `object` header, or a store that serves a tag cycle is an error, not `None`, so you can tell "not a commit" apart from a broken repository. Revision walks, fetch negotiation, upload-pack ref advertisement, bundles, commit-graph tips, `packed-refs` peel lines, tag listing and log decorations all peel through these two methods.

## Pack read caching

[`Odb`](rustdoc:grit_lib::objects::odb::Odb) owns a repository-scoped [`PackStore`](rustdoc:grit_lib::objects::pack_store::PackStore): pack directory listings, parsed `.idx` files, pack bytes, MIDX layers, and the delta-base LRU. Cloned [`Odb`](rustdoc:grit_lib::objects::odb::Odb) handles share the same store; alternate object directories get separate stores on the parent [`Odb`](rustdoc:grit_lib::objects::odb::Odb).

After repack, garbage collection, or installing a pack with [`install_pack_bytes`](rustdoc:grit_lib::objects::index_pack::install_pack_bytes), call [`Odb::invalidate_packs`](rustdoc:grit_lib::objects::odb::Odb) so the next read rescans `objects/pack/`. If another [`Odb`](rustdoc:grit_lib::objects::odb::Odb) in the same process still holds a stale listing, a lookup miss retriggers directory reprepare when the pack folder’s mtime changes.

For batch reads (`cat-file --batch`, `--batch-all-objects`), wrap the loop in [`Odb::with_pack_read_context`](rustdoc:grit_lib::objects::odb::Odb) so pack indexes, mmap-backed pack bytes, and the delta-base LRU stay on one thread-local context instead of reinstalling it per object. [`Odb::read`](rustdoc:grit_lib::objects::odb::Odb) detects an active matching context and skips nested setup.

When iterating a pack in offset order (unordered `--batch-all-objects`), prefer [`read_object_from_pack_at_offset`](rustdoc:grit_lib::objects::pack) with the entry offset from [`PackIndex`](rustdoc:grit_lib::objects::pack::PackIndex) so the read path does not repeat index lookup by OID.

## Pack deltas (encoding)

When building new pack deltas (as opposed to reusing on-disk zlib), grit-lib indexes the base blob with [`DeltaIndex`](rustdoc:grit_lib::objects::delta_encode::DeltaIndex) and encodes targets with a rolling fingerprint matcher. Reuse one index across many targets in a pack-objects window; `encode` on that index accepts an optional `max_size` cap (zero means no limit). An empty target yields a valid delta (header varints only). [`encode_delta`](rustdoc:grit_lib::objects::delta_encode::encode_delta) is a convenience wrapper for one-off base/target pairs.

## Example

This example initializes a repository, writes a blob, tree, and commit, verifies structure in memory, and prints the commit id:

<!-- include: grit-examples/src/bin/guide_objects.rs -->

Objects land on disk in standard loose format. System Git can read them:

```bash
cargo run --bin guide_objects /path/to/repo
git -C /path/to/repo fsck --strict
```

When run without arguments the binary uses a temporary repository (fine for `cargo run`, not for fsck demos).
