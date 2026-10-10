---
title: Pack reachability bitmaps
summary: Read and write Git pack `.bitmap` sidecars (EWAH reachability sets, name-hash cache, lookup tables).
---

Git can attach a **reachability bitmap** to a pack (or multi-pack-index) so rev-list and fetch paths avoid walking the full object graph. grit-lib implements the on-disk **BITM v1** format: type bitmaps, XOR-compressed commit entries, optional name-hash cache, and optional lookup table.

## Reading

[`BitmapIndex`](rustdoc:grit_lib::pack_bitmap::BitmapIndex) on a [`Repository`](rustdoc:grit_lib::repo::Repository) loads the preferred bitmap (MIDX over pack when both exist). Decoded commit reachability sets, per-kind type filters, and optional name-hash cache entries are available on the opened index (see rustdoc on [`BitmapIndex`](rustdoc:grit_lib::pack_bitmap::BitmapIndex)).

## Writing

[`PackBitmapWriter`](rustdoc:grit_lib::pack_bitmap::PackBitmapWriter) and [`Repository::write_pack_bitmap`](rustdoc:grit_lib::repo::Repository) build a `.bitmap` for an **existing** pack index whose objects are closed under reachability from the repository’s refs—the shape produced by an all-into-one repack. Options are passed explicitly as [`PackBitmapWriteOptions`](rustdoc:grit_lib::pack_bitmap::PackBitmapWriteOptions); [`ConfigSet::pack_bitmap_write_options`](rustdoc:grit_lib::config::ConfigSet) maps Git’s `pack.writeBitmapHashCache`, `pack.writeBitmapLookupTable`, and `pack.preferBitmapTips` when you want config-driven defaults.

If no `.rev` sidecar exists, the writer creates one first so object order matches Git’s pack-offset order. Output is deterministic for identical inputs. A pack that is not closed under reachability returns [`PackBitmapWriteError::NotClosed`](rustdoc:grit_lib::pack_bitmap::PackBitmapWriteError).

## Compatibility

Integration tests in `grit-lib` round-trip against the system `git` binary (`rev-list --test-bitmap`, `--use-bitmap-index` counts). Name hashes use [`pack_name_hash`](rustdoc:grit_lib::pack_name_hash::pack_name_hash) (v1) recorded during tree walks.
