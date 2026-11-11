//! Plan and apply a single-commit message rewrite (library history API demo).
//!
//! Source for the library guide "History rewrite" page.

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::progress::CancelToken;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{ApplyOptions, RefScope, RewordOptions, RewriteOptions};
use grit_lib::revwalk::{Peel, ResolveOptions};

use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository> {
    Ok(Repository::discover(
        Some(root),
        &RepositoryOptions::from_process(),
    )?)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow::anyhow!("missing repository path"))?;
    let spec = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing commit spec"))?;
    let message = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing new message"))?;

    let repo = open_repo(&root)?;
    let commit: ObjectId = repo
        .revwalk()
        .resolve(&spec, &ResolveOptions::default().with_peel(Peel::Commit))?;

    let history = repo.history();
    let plan = history.reword(
        commit,
        &RewordOptions::default().with_message(message),
        &RewriteOptions::new("History Guide <guide@grit.test> 1 +0000".parse()?)
            .with_scope(RefScope::Head),
    )?;

    for update in &plan.ref_updates {
        println!(
            "dry-run: {} {} -> {}",
            update.name,
            update.old.to_short_hex(),
            update.new.to_short_hex(),
        );
    }

    let applied = history.apply(
        &plan,
        &ApplyOptions::new(
            grit_lib::ident::parse_signature("History Guide <guide@grit.test> 1 +0000")
                .unwrap_or_else(|| panic!("guide identity must parse as a signature")),
            "history reword",
        ),
        &mut grit_lib::progress::NullProgress,
        &CancelToken::never(),
    )?;

    println!("head={}", applied.head.to_hex());
    Ok(())
}
