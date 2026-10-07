//! Walk commit history with [`grit_lib::rev_list::rev_list`] and compute a merge base.
//!
//! Source for the library guide "Revwalk" page (included in the docs site).

use grit_lib::merge_base::merge_bases_first_vs_rest;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_lib::rev_parse::{resolve_revision, split_double_dot_range};
use std::path::Path;

fn main() -> Result<(), grit_lib::error::Error> {
    let mut args = std::env::args().skip(1);
    let repo_root = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
        grit_lib::error::Error::Message("usage: guide_revwalk <repo> [main..feature]".to_owned())
    })?;
    let range = args.next().unwrap_or_else(|| "main..feature".to_owned());

    let repo = open_repo(&repo_root)?;
    let options = RevListOptions::default();
    let result = if let Some((left, right)) = split_double_dot_range(&range) {
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        if !right.is_empty() {
            positive.push(right.to_owned());
        }
        if !left.is_empty() {
            negative.push(left.to_owned());
        }
        rev_list(&repo, &positive, &negative, &options)?
    } else {
        rev_list(&repo, std::slice::from_ref(&range), &[], &options)?
    };

    for oid in &result.commits {
        println!("{oid}");
    }

    let (left_name, right_name) = range_names(&range)?;
    let left = resolve_revision(&repo, left_name)?;
    let right = resolve_revision(&repo, right_name)?;
    let bases = merge_bases_first_vs_rest(&repo, left, &[right])?;
    let Some(base) = bases.first() else {
        return Err(grit_lib::error::Error::Message(
            "no merge base for range tips".to_owned(),
        ));
    };
    println!("merge_base={base}");

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
    Repository::open(&git_dir, work_tree)
}

fn range_names(range: &str) -> Result<(&str, &str), grit_lib::error::Error> {
    split_double_dot_range(range).ok_or_else(|| {
        grit_lib::error::Error::Message(format!("expected a double-dot range, got {range:?}"))
    })
}
