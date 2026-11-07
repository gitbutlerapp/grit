// API docs: https://docs.rs/grit-lib/latest/grit_lib/repo_handles/struct.RepositoryReflogs.html
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;

fn main() -> grit_lib::error::Result<()> {
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    let head = RefName::from_trusted("HEAD");
    println!("HEAD has a reflog: {}", repo.reflogs().exists(&head)?);

    for row in repo.reflogs().entries(&head)?.iter().take(5) {
        let entry = &row.entry;
        println!("{} -> {} {}", entry.old_oid, entry.new_oid, entry.message);
    }
    Ok(())
}
