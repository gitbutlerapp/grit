//! Walk commit history with [`grit_lib::revwalk::rev_list::rev_list`] and compute a merge base.
//!
//! Source for the library guide "Revwalk" page (included in the docs site).

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::{MergeBaseMode, ResolveOptions};

use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::revwalk::rev_parse::split_double_dot_range;
use std::path::Path;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let repo_root = args
        .next()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: guide_revwalk <repo> [main..feature]"))?;
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
        repo.revwalk().rev_list(&positive, &negative, &options)?
    } else {
        repo.revwalk()
            .rev_list(std::slice::from_ref(&range), &[], &options)?
    };

    for oid in &result.commits {
        println!("{oid}");
    }

    let (left_name, right_name) = range_names(&range)?;
    let left = repo
        .revwalk()
        .resolve(left_name, &ResolveOptions::default())?;
    let right = repo
        .revwalk()
        .resolve(right_name, &ResolveOptions::default())?;
    let bases = repo
        .revwalk()
        .merge_bases(&[left, right], MergeBaseMode::Best)?;
    let Some(base) = bases.first() else {
        return Err(anyhow::anyhow!("no merge base for range tips".to_owned(),));
    };
    println!("merge_base={base}");

    Ok(())
}

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

fn range_names(range: &str) -> Result<(&str, &str), anyhow::Error> {
    split_double_dot_range(range)
        .ok_or_else(|| anyhow::anyhow!("expected a double-dot range, got {range:?}"))
}
