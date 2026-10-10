# Reachability bitmaps

> Query pack and MIDX commit bitmaps for fast object reachability and counting.

Git stores optional **reachability bitmaps** alongside pack and multi-pack-index files. grit-lib can decode those bitmaps ([`BitmapIndex`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html)) and run **want/have** reachability queries ([`ReachabilityQuery`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachabilityQuery.html), [`ReachableSet`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachableSet.html)) without walking every tree.

## Opening an index

[`BitmapIndex::open`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) prefers a MIDX bitmap when present, otherwise a pack sidecar. It returns `Ok(None)` when no valid bitmap exists.

## Reachability queries

[`BitmapIndex::reachability`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) (see [`bitmap_walk`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/index.html)) takes a repository handle, a [`ReachabilityQuery`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachabilityQuery.html) (wants, haves, optional [`ObjectFilter`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.ObjectFilter.html)), and [`MissingAction`](https://docs.rs/grit-lib/latest/grit_lib/rev_list/enum.MissingAction.html) for missing links. It returns [`ReachableSet`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachableSet.html) or [`BitmapWalkUnsupported`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.BitmapWalkUnsupported.html) when the repository is shallow or the filter needs a non-bitmap walk (`sparse:oid`, `tree:<n>` with `n > 0`).

[`ReachableSet`](https://docs.rs/grit-lib/latest/grit_lib/bitmap_walk/struct.ReachableSet.html) exposes `object_ids` for indexed and **extended** (out-of-namespace) objects, and `iter_grouped_by_kind` for oids in Git’s bitmap order (commits, trees, blobs, tags).

## Verifying on-disk bitmaps

[`BitmapIndex::verify_commit`](https://docs.rs/grit-lib/latest/grit_lib/pack_bitmap/struct.BitmapIndex.html) compares a stored commit bitmap with a fresh walk (similar to `git rev-list --test-bitmap`).

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

Run against a repository with a pack bitmap (`git repack -adb`):

```bash
cargo run --example count_reachable -- /path/to/repo HEAD
```
