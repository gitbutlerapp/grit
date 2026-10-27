//! Minimal grit-lib program: discover a repository and print HEAD's commit id.
//!
//! Source for the library quick start in the docs; kept in sync via an include directive.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::store::RefName;
use grit_lib::repo::Repository;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repo = Repository::discover(None, &RepositoryOptions::from_process())?;
    let head_name = RefName::try_from("HEAD")?;
    let head = repo.references().resolve(&head_name)?;
    let commit = repo.objects().read_commit(&head)?;
    println!("{head}");
    let subject = commit.message.lines().next().unwrap_or("");
    if !subject.is_empty() {
        println!("{subject}");
    }
    Ok(())
}
