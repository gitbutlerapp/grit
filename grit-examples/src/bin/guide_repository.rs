//! Open and discover repositories; load config from the git directory.
//!
//! Source for the library guide "Repository" page (included in the docs site).

use grit_lib::config::ConfigSet;
use grit_lib::error::Error;
use grit_lib::repo::Repository;

fn main() -> Result<(), Error> {
    let repo = match Repository::discover(None) {
        Ok(r) => r,
        Err(Error::NotARepository(_)) => {
            let here = std::env::current_dir().map_err(Error::Io)?;
            let git_dir = here.join(".git");
            Repository::open(&git_dir, Some(&here))?
        }
        Err(err) => return Err(err),
    };

    println!("git_dir={}", repo.git_dir.display());
    if let Some(wt) = &repo.work_tree {
        println!("work_tree={}", wt.display());
    } else {
        println!("work_tree=<bare>");
    }

    let cfg = ConfigSet::load(
        &grit_lib::environment::Environment::capture_process(),
        Some(&repo.git_dir),
        true,
    )?;
    let name = cfg.get("user.name").unwrap_or_default();
    if !name.is_empty() {
        println!("user.name={name}");
    }

    Ok(())
}
