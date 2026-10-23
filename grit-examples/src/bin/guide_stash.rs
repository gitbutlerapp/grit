//! Push a stash entry and list the stack through [`RepositoryStash`](grit_lib::repo_handles::RepositoryStash).
//!
//! Source for the library guide "Stash" page (included in the docs site).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::repo_handles::StashPushOptions;
use std::path::{Path, PathBuf};

const IDENT: &str = "Library Guide <guide@grit-scm.com> 1735689600 +0000";

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

fn main() -> Result<(), grit_lib::error::Error> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let repo = open_repo(&root)?;

    let pushed = repo.stash().push(&StashPushOptions {
        message: Some("library guide stash".into()),
        include_untracked: false,
        identity: IDENT.into(),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
    })?;
    println!("stash_pushed={}", pushed.is_some());

    let entries = repo.stash().list()?;
    println!("stash_count={}", entries.len());
    for entry in entries {
        println!("stash {}", entry.message);
    }

    Ok(())
}
