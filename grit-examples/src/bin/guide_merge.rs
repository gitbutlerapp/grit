//! Stored three-way merge (`write_merge_tree`) compared with `git merge-tree --write-tree`.
//!
//! Source for the library guide "Merge" page (included in the docs site).

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::ident::Signature;
use grit_lib::objects::{NewCommit, ObjectId};
use grit_lib::porcelain::merge::{write_merge_tree, MergePredictionOptions};
use grit_lib::repo::Repository;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

fn open_repo(root: &Path) -> Result<Repository> {
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
    let mut opts = RepositoryOptions::from_process();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Ok(Repository::open(&git_dir, &opts)?)
}

fn main() -> Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow::anyhow!("missing repository path"))?;
    let repo = open_repo(&root)?;

    let ours: ObjectId = repo.rev_walk().resolve("ours")?;
    let theirs: ObjectId = repo.rev_walk().resolve("theirs")?;

    // Merges over the first merge base and stores the result, like `git merge-tree --write-tree`.
    let merged = write_merge_tree(&repo, ours, theirs, &MergePredictionOptions::default())?;

    println!("merged_tree={}", merged.tree.to_hex());
    println!("conflicted_paths={}", merged.conflicts.len());
    if merged.is_clean() {
        let committer = Signature::new(
            "Guide",
            "guide@example.com",
            OffsetDateTime::from_unix_timestamp(1_700_000_000)?,
        )?;
        let commit = repo.objects().write_commit(
            &NewCommit::new(
                merged.tree,
                committer.clone(),
                committer,
                "Merge theirs into ours",
            )
            .with_parents(vec![ours, theirs]),
        )?;
        println!("merge_commit={}", commit.to_hex());
    }

    Ok(())
}
