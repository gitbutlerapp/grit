//! Initialise a repository and open it again with [`Repository::open`] and [`Repository::discover`].
//!
//! Run: `cargo run -p grit-lib --example repo_init_and_open`

use grit_lib::repo::{init_repository, Repository};
use std::fs;

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path();

    let repo = init_repository(path, false, "main", None, grit_lib::RefStorageFormat::Files)?;
    println!("opened after init: git_dir = {}", repo.git_dir.display());
    println!("work tree: {}", repo.work_tree.as_ref().unwrap().display());

    let again = Repository::open(&repo.git_dir, repo.work_tree.as_deref())?;
    println!("re-open by path: bare = {}", again.is_bare());

    let marker = path.join("marker.txt");
    fs::write(&marker, b"inside work tree\n")?;
    let sub = path.join("subdir");
    fs::create_dir_all(&sub)?;
    assert!(std::env::set_current_dir(&sub).is_ok());
    let discovered = Repository::discover(None)?;
    println!(
        "discover from subdir: same git_dir = {}",
        discovered.git_dir == repo.git_dir
    );

    Ok(())
}
