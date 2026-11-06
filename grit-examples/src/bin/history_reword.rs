//! Plan and apply a single-commit message rewrite (library history API demo).
//!
//! Source for the library guide "History rewrite" page.

use anyhow::Result;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, reword, ApplyOptions, EmptyPolicy, RefScope, RewriteOptions,
};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
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
    let commit: ObjectId = resolve_revision_as_commit(&repo, &spec)?;

    let plan = reword(
        &repo,
        commit,
        message,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: "History Guide <guide@grit.test> 1 +0000".parse()?,
            preserve_committer: false,
            sign: true,
        },
    )?;

    for update in &plan.ref_updates {
        println!(
            "dry-run: {} {} -> {}",
            update.name,
            update.old.to_short_hex(),
            update.new.to_short_hex(),
        );
    }

    let applied = apply_rewrite(
        &repo,
        &plan,
        &ApplyOptions {
            identity: grit_lib::ident::parse_signature("History Guide <guide@grit.test> 1 +0000")
                .unwrap_or_else(|| panic!("guide identity must parse as a signature")),
            reflog_message: "history reword".into(),
            record_operation: None,
            hooks: grit_lib::command_runner::hooks::PostOperationHooks::Run,
        },
        &mut grit_lib::progress::NullProgress,
        &CancelToken::never(),
    )?;

    println!("head={}", applied.head.to_hex());
    Ok(())
}
