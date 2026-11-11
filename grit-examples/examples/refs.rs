// API docs: https://docs.rs/grit-lib/latest/grit_lib/refs/struct.References.html
//
// Network fetch paths reject malformed advertised ref names via
// `grit_lib::refs::is_valid_fetch_advertised_ref` before refspec mapping.
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::store::RefNameRef;
use grit_lib::repo::Repository;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    let refs = repo.references();
    println!("HEAD = {:?}", refs.symbolic(RefNameRef::HEAD)?);

    for (name, oid) in refs.list("refs/heads/")? {
        println!("{name} {oid}");
    }
    Ok(())
}
