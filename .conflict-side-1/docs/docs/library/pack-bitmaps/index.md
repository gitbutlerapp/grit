# Pack reachability bitmaps

> Read and write Git pack `.bitmap` sidecars (EWAH reachability sets, name-hash cache, lookup tables).

Git can attach a **reachability bitmap** to a pack (or multi-pack-index) so rev-list and fetch paths avoid walking the full object graph. grit-lib implements the on-disk **BITM v1** format: type bitmaps, XOR-compressed commit entries, optional name-hash cache, and optional lookup table.

## Reading

[`BitmapIndex`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) on a [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) loads the preferred bitmap (MIDX over pack when both exist). Decoded commit reachability sets, per-kind type filters, and optional name-hash cache entries are available on the opened index (see rustdoc on [`BitmapIndex`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html)).

## Writing

[`PackBitmapWriter`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.PackBitmapWriter.html) and [`Repository::write_pack_bitmap`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) build a `.bitmap` for an **existing** pack index whose objects are closed under reachability from the repository’s refs—the shape produced by an all-into-one repack. Options are passed explicitly as [`PackBitmapWriteOptions`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.PackBitmapWriteOptions.html); [`ConfigSet::pack_bitmap_write_options`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html) maps Git’s `pack.writeBitmapHashCache`, `pack.writeBitmapLookupTable`, and `pack.preferBitmapTips` when you want config-driven defaults.

If no `.rev` sidecar exists, the writer creates one first so object order matches Git’s pack-offset order. Output is deterministic for identical inputs. A pack that is not closed under reachability returns [`PackBitmapWriteError::NotClosed`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/enum.PackBitmapWriteError.html).

## Compatibility

Integration tests in `grit-lib` round-trip against the system `git` binary (`rev-list --test-bitmap`, `--use-bitmap-index` counts). Name hashes use [`pack_name_hash`](https://docs.rs/grit-lib/latest/grit_lib/pack_name_hash/fn.pack_name_hash.html) (v1) recorded during tree walks.
