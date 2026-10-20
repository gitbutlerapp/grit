//! List commits that touched a file, following renames (`RevWalk::follow`).
//!
//! Source for the library guide "Revwalk" follow section.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::follow::{FollowWalkOptions, FollowedCommit};
use grit_lib::revwalk::RevWalkListOptions;
use std::path::Path;

fn main() -> Result<(), grit_lib::error::Error> {
    let mut args = std::env::args().skip(1);
    let repo_root = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
        grit_lib::error::Error::Message(
            "usage: guide_follow_log <repo> <path> [max-count]".to_owned(),
        )
    })?;
    let path = args
        .next()
        .ok_or_else(|| grit_lib::error::Error::Message("missing path".to_owned()))?;
    let limit = args.next().and_then(|s| s.parse().ok());

    let repo = open_repo(&repo_root)?;
    let opts = FollowWalkOptions {
        walk: RevWalkListOptions {
            limit,
            ..RevWalkListOptions::default()
        },
        ..FollowWalkOptions::default()
    };
    let walk = repo.rev_walk().follow(&["HEAD"], &path, &opts)?;
    for FollowedCommit { oid, path } in walk {
        println!("{oid} {path}");
    }
    Ok(())
}

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    let mut opts = RepositoryOptions::empty();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Repository::open(&git_dir, &opts)
}
