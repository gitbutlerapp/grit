---
title: Pack reachability bitmaps
summary: Read and write Git pack `.bitmap` sidecars (EWAH reachability sets, name-hash cache, lookup tables).
---

Git can attach a **reachability bitmap** to a pack (or multi-pack-index) so rev-list and fetch paths avoid walking the full object graph. grit-lib implements the on-disk **BITM v1** format: type bitmaps, XOR-compressed commit entries, optional name-hash cache, and optional lookup table.

## Reading

[`BitmapIndex`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) on a [`Repository`](rustdoc:grit_lib::repo::Repository) loads the preferred bitmap (MIDX over pack when both exist). Decoded commit reachability sets, per-kind type filters, and optional name-hash cache entries are available on the opened index (see rustdoc on [`BitmapIndex`](rustdoc:grit_lib::objects::bitmap::BitmapIndex)).

## Writing

[`PackBitmapWriter`](rustdoc:grit_lib::objects::bitmap::PackBitmapWriter) and [`Repository::write_largest_pack_bitmap`](rustdoc:grit_lib::repo::Repository) build a `.bitmap` for an **existing** pack index whose objects are closed under reachability from the repository’s refs—the shape produced by an all-into-one repack. Options are passed explicitly as [`PackBitmapWriteOptions`](rustdoc:grit_lib::objects::bitmap::PackBitmapWriteOptions); [`ConfigSet::pack_bitmap_write_options`](rustdoc:grit_lib::config::ConfigSet) maps Git’s `pack.writeBitmapHashCache`, `pack.writeBitmapLookupTable`, and `pack.preferBitmapTips` when you want config-driven defaults.

If no `.rev` sidecar exists, the writer creates one first so object order matches Git’s pack-offset order. Output is deterministic for identical inputs. A pack that is not closed under reachability returns [`PackBitmapWriteError::NotClosed`](rustdoc:grit_lib::objects::bitmap::PackBitmapWriteError).

The writer only inflates commits and trees. Object types come from pack entry headers, tree entries are classified by mode (blobs are never read), and each commit is parsed once. Selected commits are filled oldest first, seeded from bitmaps already computed for their ancestors. Where the first-parent histories of two selected commits meet (for example, a release branch forking from the main line), the writer computes an unwritten bitmap at the fork, so the history below it is walked once. Closure is checked during that strict walk, plus a check of the tags, trees and blobs that refs name directly. On `rust-lang/cargo` (195k objects, 277 bitmaps) a write takes about 1 s; see [Benchmarks](../benchmarks.md#grit-lib-writing-pack-bitmaps).

A blob's name-hash entry is the path from the newest tree that named it when that tree was first walked. A tree keeps the first path it was reached by. Like Git's, these hashes are only delta-search hints.

## Compatibility

Integration tests in `grit-lib` round-trip against the system `git` binary (`rev-list --test-bitmap`, `--use-bitmap-index` counts). Name hashes use [`pack_name_hash`](rustdoc:grit_lib::objects::pack_name_hash::pack_name_hash) (v1) recorded during tree walks.
