---
title: Reachability bitmaps
summary: Query pack and MIDX commit bitmaps for fast object reachability and counting.
---

Git stores optional **reachability bitmaps** alongside pack and multi-pack-index files. grit-lib can decode those bitmaps ([`BitmapIndex`](rustdoc:grit_lib::pack_bitmap::BitmapIndex)) and run **want/have** reachability queries ([`ReachabilityQuery`](rustdoc:grit_lib::bitmap_walk::ReachabilityQuery), [`ReachableSet`](rustdoc:grit_lib::bitmap_walk::ReachableSet)) without walking every tree.

## Opening an index

[`BitmapIndex::open`](rustdoc:grit_lib::pack_bitmap::BitmapIndex) prefers a MIDX bitmap when present, otherwise a pack sidecar. It returns `Ok(None)` when no valid bitmap exists.

## Reachability queries

[`BitmapIndex::reachability`](rustdoc:grit_lib::pack_bitmap::BitmapIndex) (see [`bitmap_walk`](rustdoc:grit_lib::bitmap_walk)) takes a repository handle, a [`ReachabilityQuery`](rustdoc:grit_lib::bitmap_walk::ReachabilityQuery) (wants, haves, optional [`ObjectFilter`](rustdoc:grit_lib::rev_list::ObjectFilter)), and [`MissingAction`](rustdoc:grit_lib::rev_list::MissingAction) for missing links. It returns [`ReachableSet`](rustdoc:grit_lib::bitmap_walk::ReachableSet) or [`BitmapWalkUnsupported`](rustdoc:grit_lib::bitmap_walk::BitmapWalkUnsupported) when the repository is shallow or the filter needs a non-bitmap walk (`sparse:oid`, `tree:<n>` with `n > 0`).

[`ReachableSet`](rustdoc:grit_lib::bitmap_walk::ReachableSet) exposes `object_ids` for indexed and **extended** (out-of-namespace) objects, and `iter_grouped_by_kind` for oids in Git’s bitmap order (commits, trees, blobs, tags).

## Verifying on-disk bitmaps

[`BitmapIndex::verify_commit`](rustdoc:grit_lib::pack_bitmap::BitmapIndex) compares a stored commit bitmap with a fresh walk (similar to `git rev-list --test-bitmap`).

## Example

<!-- include: grit-examples/examples/count_reachable.rs -->

Run against a repository with a pack bitmap (`git repack -adb`):

```bash
cargo run --example count_reachable -- /path/to/repo HEAD
```
