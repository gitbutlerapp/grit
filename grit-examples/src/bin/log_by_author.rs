//! List commits matching an author pattern via [`CommitFilter`](grit_lib::revwalk::CommitFilter) and [`RevWalk::rev_list`](grit_lib::revwalk::RevWalk::rev_list).
//!
//! Source for the library guide "Revwalk" page (included in the docs site).

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_list::RevListOptions;
use grit_lib::revwalk::CommitFilterBuilder;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let repo_root = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
        anyhow::anyhow!("usage: log_by_author <repo> <author-pattern> [revision…]".to_owned(),)
    })?;
    let author = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing author pattern"))?;
    let tips: Vec<String> = args.collect();
    let tips = if tips.is_empty() {
        vec!["HEAD".to_owned()]
    } else {
        tips
    };

    let repo = open_repo(&repo_root)?;
    let mut builder = CommitFilterBuilder::new().author(author);
    if let Ok(table) = repo.config().mailmap() {
        if !table.is_empty() {
            builder = builder.mailmap(table);
        }
    }
    let filter = Arc::new(builder.build()?);
    let opts = RevListOptions {
        commit_filter: Some(filter),
        ..RevListOptions::default()
    };
    let result = repo.revwalk().rev_list(&tips, &[], &opts)?;
    for oid in &result.commits {
        println!("{oid}");
    }
    Ok(())
}

fn open_repo(root: &Path) -> Result<Repository> {
    Ok(Repository::discover(
        Some(root),
        &RepositoryOptions::from_process(),
    )?)
}
