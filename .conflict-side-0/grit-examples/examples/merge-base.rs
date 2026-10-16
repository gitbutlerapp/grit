// API docs: https://docs.rs/grit-lib/latest/grit_lib/merge_base/index.html
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::merge_base::resolve_fork_point_reflog_ref;
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(Some(Path::new(".")), &RepositoryOptions::empty())?;
    let reflog_ref = resolve_fork_point_reflog_ref(&repo, "main");

    println!("look for fork-point entries in {reflog_ref}");
    Ok(())
}
