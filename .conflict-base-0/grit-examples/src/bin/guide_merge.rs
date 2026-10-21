//! Three-way tree merge compared with `git merge-tree --write-tree`.
//!
//! Source for the library guide "Merge" page (included in the docs site).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::merge_file::MergeFavor;
use grit_lib::diff::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::merge_bases_first_vs_rest;
use std::path::{Path, PathBuf};

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

fn tree_of(repo: &Repository, commit: &ObjectId) -> Result<ObjectId, grit_lib::error::Error> {
    let obj = repo.objects().read(commit)?;
    Ok(parse_commit(&obj.data)?.tree)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;

    let ours: ObjectId = repo.rev_walk().resolve("ours")?;
    let theirs: ObjectId = repo.rev_walk().resolve("theirs")?;
    let base = merge_bases_first_vs_rest(&repo, ours, &[theirs])?
        .into_iter()
        .next()
        .ok_or_else(|| grit_lib::error::Error::Message("no merge base".into()))?;

    let base_tree = tree_of(&repo, &base)?;
    let ours_tree = tree_of(&repo, &ours)?;
    let theirs_tree = tree_of(&repo, &theirs)?;

    let merged = merge_trees_three_way(
        &repo,
        base_tree,
        ours_tree,
        theirs_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )?;

    let result_tree = merged.result_tree;
    println!("merged_tree={}", result_tree.to_hex());
    println!("unmerged_paths={}", merged.conflict_content.len());

    Ok(())
}
