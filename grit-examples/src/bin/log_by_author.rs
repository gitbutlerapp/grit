//! List commits matching an author pattern via [`CommitFilter`] and [`rev_list`].
//!
//! Source for the library guide "Revwalk" page (included in the docs site).

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::mailmap::load_mailmap_table;
use grit_lib::repo::Repository;
use grit_lib::revwalk::filter::CommitFilterBuilder;
use grit_lib::revwalk::rev_list::{rev_list, RevListOptions};

fn main() -> Result<(), grit_lib::error::Error> {
    let mut args = std::env::args().skip(1);
    let repo_root = args.next().map(std::path::PathBuf::from).ok_or_else(|| {
        grit_lib::error::Error::Message(
            "usage: log_by_author <repo> <author-pattern> [revision…]".to_owned(),
        )
    })?;
    let author = args
        .next()
        .ok_or_else(|| grit_lib::error::Error::Message("missing author pattern".to_owned()))?;
    let tips: Vec<String> = args.collect();
    let tips = if tips.is_empty() {
        vec!["HEAD".to_owned()]
    } else {
        tips
    };

    let repo = open_repo(&repo_root)?;
    let mut builder = CommitFilterBuilder::new().author(author);
    if let Ok(table) = load_mailmap_table(&repo) {
        if !table.is_empty() {
            builder = builder.mailmap(table);
        }
    }
    let filter = Arc::new(
        builder
            .build()
            .map_err(|e| grit_lib::error::Error::Message(e.to_string()))?,
    );
    let opts = RevListOptions {
        commit_filter: Some(filter),
        ..RevListOptions::default()
    };
    let result = rev_list(&repo, &tips, &[], &opts)?;
    for oid in &result.commits {
        println!("{oid}");
    }
    Ok(())
}

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    Repository::discover(Some(root), &RepositoryOptions::empty())
}
