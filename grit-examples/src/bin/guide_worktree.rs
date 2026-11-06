//! List linked worktrees and match Git's worktree admin output.
//!
//! Source for the library guide "Worktree" page (included in the docs site).

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use std::path::{Path, PathBuf};

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

    let rows = repo.worktrees().list()?;
    println!("worktree_count={}", rows.len());
    for entry in rows {
        println!("worktree {}", entry.path.display());
    }

    Ok(())
}
