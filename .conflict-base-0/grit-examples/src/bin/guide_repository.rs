//! Open and discover repositories; load config from the git directory.
//!
//! Source for the library guide "Repository" page (included in the docs site).

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::repo::Repository;

fn main() -> Result<(), Error> {
    let cwd = std::env::current_dir().map_err(Error::Io)?;
    let env = Environment::from_vars(std::env::vars_os(), cwd.clone());
    let options = RepositoryOptions::with_environment(env.clone());

    let repo = match Repository::discover(None, &options) {
        Ok(r) => r,
        Err(Error::NotARepository(_)) => {
            let git_dir = cwd.join(".git");
            Repository::open(&git_dir, &options.clone().with_work_tree(&cwd))?
        }
        Err(err) => return Err(err),
    };

    println!("git_dir={}", repo.git_dir().display());
    if let Some(wt) = &repo.work_tree() {
        println!("work_tree={}", wt.display());
    } else {
        println!("work_tree=<bare>");
    }

    let name = repo.config_handle().get("user.name")?.unwrap_or_default();
    if !name.is_empty() {
        println!("user.name={name}");
    }

    Ok(())
}
