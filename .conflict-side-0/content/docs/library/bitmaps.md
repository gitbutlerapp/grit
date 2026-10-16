---
title: Reachability bitmaps
summary: Query pack and MIDX commit bitmaps for fast object reachability and counting.
---

Git stores optional **reachability bitmaps** alongside pack and multi-pack-index (MIDX) files. grit-lib reads and writes Git-compatible **BITM v1** sidecars (EWAH-compressed sets, XOR commit chains, optional name-hash cache and lookup table). Use them for fast rev-list counting, fetch negotiation, and pack generation without walking the full object graph on every request.

For pack-only write options and on-disk layout details, see the **pack-bitmaps** guide page in this section (`content/docs/library/pack-bitmaps.md`).

## Reading pack and MIDX bitmaps

[`BitmapIndex::open`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) on a [`Repository`](rustdoc:grit_lib::repo::Repository) picks the active index: **MIDX bitmap when present**, otherwise the largest pack’s `.bitmap`. It returns `Ok(None)` when no valid sidecar exists (missing file, zero-byte placeholder, or corrupt header).

After open:

- [`BitmapIndex::commit_bitmap`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) decodes one commit’s reachability set (type-filtered EWAH).
- [`BitmapIndex::oid_at`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) / [`object_count`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) map pack positions to object ids.
- Optional [`name_hash`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) entries come from the name-hash cache (path hashes during tree walks).
- [`has_lookup_table`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) reports whether a lookup table chunk is present.

Integration tests in `grit-lib/tests/pack_bitmap_read_git_compat.rs` round-trip **git-written** and **grit-written** pack bitmaps and MIDX bitmaps against `git rev-list --test-bitmap`.

## Querying reachability

[`BitmapIndex::reachability`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) (see the [`objects::bitmap`](rustdoc:grit_lib::objects::bitmap) domain module) takes a [`ReachabilityQuery`](rustdoc:grit_lib::objects::bitmap::ReachabilityQuery) (wants, haves, optional [`ObjectFilter`](rustdoc:grit_lib::revwalk::rev_list::ObjectFilter)), and [`MissingAction`](rustdoc:grit_lib::revwalk::rev_list::MissingAction) for missing links. It returns [`ReachableSet`](rustdoc:grit_lib::objects::bitmap::ReachableSet) or [`BitmapWalkUnsupported`](rustdoc:grit_lib::objects::bitmap::BitmapWalkUnsupported) when the repository is shallow or the filter needs a non-bitmap walk (`sparse:oid`, `tree:<n>` with `n > 0`, and similar).

[`ReachableSet`](rustdoc:grit_lib::objects::bitmap::ReachableSet) exposes `object_ids` for indexed and **extended** (out-of-namespace) objects, and `iter_grouped_by_kind` for oids in Git’s bitmap order (commits, trees, blobs, tags).

[`BitmapIndex::verify_commit`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) compares a stored commit bitmap with a fresh non-bitmap walk (same idea as `git rev-list --test-bitmap`).

## Writing pack bitmaps

[`PackBitmapWriter`](rustdoc:grit_lib::objects::bitmap::PackBitmapWriter) and [`Repository::write_largest_pack_bitmap`](rustdoc:grit_lib::repo::Repository) build a `.bitmap` for an **existing** pack index whose objects are closed under reachability from the repository’s refs (typical after `git repack -ad`). Pass [`PackBitmapWriteOptions`](rustdoc:grit_lib::objects::bitmap::PackBitmapWriteOptions) explicitly, or use [`ConfigSet::pack_bitmap_write_options`](rustdoc:grit_lib::config::ConfigSet) for Git’s `pack.writeBitmapHashCache`, `pack.writeBitmapLookupTable`, and `pack.preferBitmapTips`.

If no `.rev` sidecar exists, the writer creates one so object order matches Git’s pack-offset order. Output is deterministic for identical inputs. A pack that is not closed under reachability returns [`PackBitmapWriteError::NotClosed`](rustdoc:grit_lib::objects::bitmap::PackBitmapWriteError).

## MIDX bitmaps

When grit writes or compacts a multi-pack-index with bitmap support enabled, it emits real MIDX `.bitmap` chunks (and matching `.rev` sidecars) instead of zero-byte placeholders. [`BitmapIndex::open`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) prefers that MIDX bitmap for reachability queries. See `grit-lib/tests/midx_write_coverage.rs` and `reads_git_midx_bitmap` in `pack_bitmap_read_git_compat.rs`.

## Fallback semantics

When bitmaps are missing, truncated, or unsupported for the requested walk:

- [`BitmapIndex::open`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) returns `Ok(None)`; callers fall back to graph walks.
- Corrupt sidecars may emit diagnostics (for example EWAH or index corruption) while still completing the operation via fallback where Git would.
- [`rev_list`](rustdoc:grit_lib::revwalk::rev_list::rev_list) uses bitmap fast paths when [`RevListOptions`](rustdoc:grit_lib::revwalk::rev_list::RevListOptions) allow it (`--use-bitmap-index` semantics); incompatible options or shallow repos fall back transparently (see `grit-lib/tests/rev_list_bitmaps.rs`).
- [`build_pack`](rustdoc:grit_lib::objects::pack_objects::build_pack) and upload-pack use [`enumerate_pack_objects`](rustdoc:grit_lib::objects::pack_object_select::enumerate_pack_objects) when [`PackBuildOptions::use_bitmaps`](rustdoc:grit_lib::objects::pack_objects::PackBuildOptions) is true and a [`BitmapIndex`](rustdoc:grit_lib::objects::bitmap::BitmapIndex) is available; otherwise the object walk runs with the same resulting set.

## Not supported yet

- **Pseudo-merge** and other experimental bitmap encodings Git may gate behind feature flags.
- **Incremental MIDX** bitmap chains (Git rejects `repack -d` with bitmaps unless `--no-write-bitmap-index`; grit does not write bitmaps for incremental repacks).
- UX-only upstream cases (`--progress`, JGit interop, `pack.preferBitmapTips` misconfiguration messages) are not CLI goals; library APIs accept explicit tip lists via `PackBitmapWriteOptions`.

## Example

<!-- include: grit-examples/examples/count_reachable.rs -->

Run against a repository with a pack bitmap (`git repack -adb` or grit’s writer):

```bash
cargo run --example count_reachable -- /path/to/repo HEAD
```
