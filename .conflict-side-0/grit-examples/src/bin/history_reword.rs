//! Plan and apply a single-commit message rewrite (library history API demo).
//!
//! Source for the library guide "History rewrite" page.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, reword, ApplyOptions, EmptyPolicy, RefScope, RewriteOptions,
};
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    Repository::discover(Some(root), &RepositoryOptions::empty())
}

fn main() -> Result<(), grit_lib::error::Error> {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| grit_lib::error::Error::Message("missing repository path".into()))?;
    let spec = args
        .next()
        .ok_or_else(|| grit_lib::error::Error::Message("missing commit spec".into()))?;
    let message = args
        .next()
        .ok_or_else(|| grit_lib::error::Error::Message("missing new message".into()))?;

    let repo = open_repo(&root)?;
    let commit: ObjectId = resolve_revision_as_commit(&repo, &spec)?;

    let plan = reword(
        &repo,
        commit,
        message,
        &RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: "History Guide <guide@grit.test> 1 +0000".into(),
            preserve_committer: false,
            sign: true,
        },
    )
    .map_err(grit_lib::error::Error::History)?;

    for line in plan.ref_update_lines() {
        println!("dry-run: {line}");
    }

    let applied = apply_rewrite(
        &repo,
        &plan,
        &ApplyOptions {
            identity: "History Guide <guide@grit.test>".into(),
            reflog_message: "history reword".into(),
            record_operation: None,
        },
    )
    .map_err(grit_lib::error::Error::History)?;

    println!("head={}", applied.head.to_hex());
    Ok(())
}
