// API docs: https://docs.rs/grit-lib/latest/grit_lib/refs/struct.References.html#method.head
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_lib::state::HeadState;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    match repo.references().head()? {
        HeadState::Branch { refname, oid, .. } => println!("{refname} at {oid:?}"),
        HeadState::Detached { oid } => println!("detached at {oid}"),
        HeadState::Invalid => println!("HEAD is invalid"),
    }
    Ok(())
}
