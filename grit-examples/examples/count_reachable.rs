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
