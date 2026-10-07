//! Incremental cache-tree update for an on-disk repo (benchmark helper).
//!
//! Loads the index, runs [`write_tree_update_index`] with silent ODB writes, persists the
//! cache-tree extension, and prints the root tree OID on stdout.
//!
//! Run: `cargo run --release -p grit-lib --example write_tree_update -- /path/to/repo`

use std::env;
use std::path::Path;

use grit_lib::repo::Repository;
use grit_lib::write_tree::{write_tree_update_index, WriteTreeFlags};

fn main() -> grit_lib::error::Result<()> {
    let Some(arg) = env::args().nth(1) else {
        eprintln!("usage: write_tree_update <repo-worktree-path>");
        std::process::exit(2);
    };
    let path = Path::new(&arg);

    let repo = Repository::discover(Some(&path))?;
    let mut index = repo.load_index()?;
    let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())?;
    repo.write_index(&mut index)?;
    println!("{tree}");
    Ok(())
}
