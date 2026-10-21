# Reachability bitmaps

> Query pack and MIDX commit bitmaps for fast object reachability and counting.

Git stores optional **reachability bitmaps** alongside pack and multi-pack-index (MIDX) files. grit-lib reads and writes Git-compatible **BITM v1** sidecars (EWAH-compressed sets, XOR commit chains, optional name-hash cache and lookup table). Use them for fast rev-list counting, fetch negotiation, and pack generation without walking the full object graph on every request.

For pack-only write options and on-disk layout details, see the **pack-bitmaps** guide page in this section (`content/docs/library/pack-bitmaps.md`).

## Reading pack and MIDX bitmaps

[`BitmapIndex::open`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) on a [`Repository`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) picks the active index: **MIDX bitmap when present**, otherwise the largest pack’s `.bitmap`. It returns `Ok(None)` when no valid sidecar exists (missing file, zero-byte placeholder, or corrupt header).

After open:

- [`BitmapIndex::commit_bitmap`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) decodes one commit’s reachability set (type-filtered EWAH).
- [`BitmapIndex::oid_at`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) / [`object_count`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) map pack positions to object ids.
- Optional [`name_hash`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) entries come from the name-hash cache (path hashes during tree walks).
- [`has_lookup_table`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) reports whether a lookup table chunk is present.

Integration tests in `grit-lib/tests/pack_bitmap_read_git_compat.rs` round-trip **git-written** and **grit-written** pack bitmaps and MIDX bitmaps against `git rev-list --test-bitmap`.

## Querying reachability

[`BitmapIndex::reachability`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) takes a [`ReachabilityQuery`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachabilityQuery.html) (wants, haves, optional [`ObjectFilter`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.ObjectFilter.html)), and [`MissingAction`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.MissingAction.html) for missing links. It returns [`ReachableSet`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachableSet.html) or [`BitmapWalkUnsupported`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.BitmapWalkUnsupported.html) when the repository is shallow or the filter needs a non-bitmap walk (`sparse:oid`, `tree:<n>` with `n > 0`, and similar).

[`ReachableSet`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachableSet.html) exposes `object_ids` for indexed and **extended** (out-of-namespace) objects, and `iter_grouped_by_kind` for oids in Git’s bitmap order (commits, trees, blobs, tags).

[`BitmapIndex::verify_commit`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) compares a stored commit bitmap with a fresh non-bitmap walk (same idea as `git rev-list --test-bitmap`).

## Writing pack bitmaps

[`PackBitmapWriter`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.PackBitmapWriter.html) and [`Repository::write_pack_bitmap`](https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html) build a `.bitmap` for an **existing** pack index whose objects are closed under reachability from the repository’s refs (typical after `git repack -ad`). Pass [`PackBitmapWriteOptions`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.PackBitmapWriteOptions.html) explicitly, or use [`ConfigSet::pack_bitmap_write_options`](https://docs.rs/grit-lib/latest/grit_lib/config/struct.ConfigSet.html) for Git’s `pack.writeBitmapHashCache`, `pack.writeBitmapLookupTable`, and `pack.preferBitmapTips`.

If no `.rev` sidecar exists, the writer creates one so object order matches Git’s pack-offset order. Output is deterministic for identical inputs. A pack that is not closed under reachability returns [`PackBitmapWriteError::NotClosed`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/enum.PackBitmapWriteError.html).

## MIDX bitmaps

When grit writes or compacts a multi-pack-index with bitmap support enabled, it emits real MIDX `.bitmap` chunks (and matching `.rev` sidecars) instead of zero-byte placeholders. [`BitmapIndex::open`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) prefers that MIDX bitmap for reachability queries. See `grit-lib/tests/midx_write_coverage.rs` and `reads_git_midx_bitmap` in `pack_bitmap_read_git_compat.rs`.

## Fallback semantics

When bitmaps are missing, truncated, or unsupported for the requested walk:

- [`BitmapIndex::open`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) returns `Ok(None)`; callers fall back to graph walks.
- Corrupt sidecars may emit diagnostics (for example EWAH or index corruption) while still completing the operation via fallback where Git would.
- [`rev_list`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/fn.rev_list.html) uses bitmap fast paths when [`RevListOptions`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/struct.RevListOptions.html) allow it (`--use-bitmap-index` semantics); incompatible options or shallow repos fall back transparently (see `grit-lib/tests/rev_list_bitmaps.rs`).
- [`build_pack`](https://docs.rs/grit-lib/latest/grit_lib/pack_objects/fn.build_pack.html) and upload-pack use [`enumerate_pack_objects`](https://docs.rs/grit-lib/latest/grit_lib/pack_object_select/fn.enumerate_pack_objects.html) when [`PackBuildOptions::use_bitmaps`](https://docs.rs/grit-lib/latest/grit_lib/pack_objects/struct.PackBuildOptions.html) is true and a [`BitmapIndex`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) is available; otherwise the object walk runs with the same resulting set.

## Not supported yet

- **Pseudo-merge** and other experimental bitmap encodings Git may gate behind feature flags.
- **Incremental MIDX** bitmap chains (Git rejects `repack -d` with bitmaps unless `--no-write-bitmap-index`; grit does not write bitmaps for incremental repacks).
- UX-only upstream cases (`--progress`, JGit interop, `pack.preferBitmapTips` misconfiguration messages) are not CLI goals; library APIs accept explicit tip lists via `PackBitmapWriteOptions`.

## Example

```rust
//! Count objects reachable from a commit using pack/MIDX bitmaps when available.
//!
//! Usage: `cargo run --example count_reachable -- <repo> <commit>`

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use grit_lib::bitmap_walk::{BitmapWalkError, ReachabilityQuery};
use grit_lib::objects::ObjectId;
use grit_lib::pack_bitmap::BitmapIndex;
use grit_lib::repo::Repository;
use grit_lib::rev_list::MissingAction;
use grit_lib::rev_parse::resolve_revision_for_range_end;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(repo_path) = args.next() else {
        eprintln!("usage: count_reachable <repo> <commit>");
        return ExitCode::from(2);
    };
    let Some(rev) = args.next() else {
        eprintln!("usage: count_reachable <repo> <commit>");
        return ExitCode::from(2);
    };

    let repo = match Repository::discover(Some(repo_path.as_ref())) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("open repository: {err}");
            return ExitCode::from(1);
        }
    };
    let commit: ObjectId = match resolve_revision_for_range_end(&repo, &rev) {
        Ok(oid) => oid,
        Err(err) => {
            eprintln!("resolve {rev}: {err}");
            return ExitCode::from(1);
        }
    };

    let index: Arc<BitmapIndex> = match BitmapIndex::open(&repo) {
        Ok(Some(idx)) => idx,
        Ok(None) => {
            eprintln!("no reachability bitmap in this repository");
            return ExitCode::from(1);
        }
        Err(err) => {
            eprintln!("open bitmap: {err}");
            return ExitCode::from(1);
        }
    };

    let query = ReachabilityQuery {
        wants: &[commit],
        haves: &[],
        filter: None,
    };
    match index.reachability(&repo, query, MissingAction::Error) {
        Ok(set) => {
            println!("reachable objects: {}", set.count());
            println!(
                "  commits: {}",
                set.count_by_kind(grit_lib::objects::ObjectKind::Commit)
                    .unwrap_or(0)
            );
            println!(
                "  trees: {}",
                set.count_by_kind(grit_lib::objects::ObjectKind::Tree)
                    .unwrap_or(0)
            );
            println!(
                "  blobs: {}",
                set.count_by_kind(grit_lib::objects::ObjectKind::Blob)
                    .unwrap_or(0)
            );
            println!(
                "  tags: {}",
                set.count_by_kind(grit_lib::objects::ObjectKind::Tag)
                    .unwrap_or(0)
            );
            ExitCode::SUCCESS
        }
        Err(BitmapWalkError::Unsupported(_)) => {
            eprintln!("bitmap walk unsupported for this query (use rev-list without bitmaps)");
            ExitCode::from(1)
        }
        Err(err) => {
            eprintln!("bitmap walk failed: {err}");
            ExitCode::from(1)
        }
    }
}
```

Run against a repository with a pack bitmap (`git repack -adb` or grit’s writer):

```bash
cargo run --example count_reachable -- /path/to/repo HEAD
```
