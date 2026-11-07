// API docs: https://docs.rs/grit-lib/latest/grit_lib/revwalk/struct.RevWalk.html
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::revwalk::{Peel, ResolveOptions};
use std::path::Path;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(Some(Path::new(".")), &RepositoryOptions::from_process())?;
    let revwalk = repo.revwalk();
    let commit = ResolveOptions::default().with_peel(Peel::Commit);
    let upstream = revwalk.resolve("main", &commit)?;
    let head = revwalk.resolve("HEAD", &commit)?;
    match revwalk.fork_point("main", upstream, head)? {
        Some(fork_point) => println!("HEAD forked from main at {fork_point}"),
        None => println!("main's reflog has no fork point for HEAD"),
    }
    Ok(())
}
