//! Print the tree-sha256 digest for `HEAD` (or the first argument).
//!
//! Source for the library guide "Tree SHA-256" page.

use grit_lib::error::Error;
use grit_lib::revwalk::rev_parse::resolve_revision_as_commit;
use grit_lib::tree_sha256::TreeSha256Options;

fn main() -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(Error::Io)?;
    let env = grit_lib::config::environment::Environment::from_vars(std::env::vars_os(), cwd);
    let options = grit_lib::config::environment::RepositoryOptions::with_environment(env);
    let repo = grit_lib::repo::Repository::discover(None, &options)?;
    let rev = std::env::args().nth(1).unwrap_or_else(|| "HEAD".into());
    let oid = resolve_revision_as_commit(&repo, &rev)?;
    let report = repo.tree_sha256(oid, &TreeSha256Options::default())?;
    println!("{}", report.digest);
    println!("files={} submodules={}", report.files, report.submodules);
    Ok(())
}
