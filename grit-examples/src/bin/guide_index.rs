//! Read the index, stage paths, and write a tree from staged entries.
//!
//! Source for the library guide "Index" page (included in the docs site).

use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{init_repository, Repository};
use grit_lib::write_tree::write_tree_from_index;
use std::fs;
use std::path::{Path, PathBuf};

fn open_repo(root: &Path) -> Result<Repository, grit_lib::error::Error> {
    let git_dir = if root.join(".git").is_dir() {
        root.join(".git")
    } else {
        root.to_path_buf()
    };
    let work_tree = if root.join(".git").is_dir() {
        Some(root)
    } else {
        None
    };
    Repository::open(&git_dir, work_tree)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let mut temp_guard = None;
    let repo = if let Some(root) = std::env::args().nth(1).map(PathBuf::from) {
        open_repo(&root)?
    } else {
        let temp = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
        init_repository(temp.path(), false, "main", None, "files")?;
        let path = temp.path().join("hello.txt");
        fs::write(&path, b"staged from the library guide\n").map_err(grit_lib::error::Error::Io)?;
        let opened = open_repo(temp.path())?;
        temp_guard = Some(temp);
        opened
    };
    let _keep = temp_guard;

    let index = repo.load_index()?;
    println!("index_entries={}", index.entries.len());

    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress)?;
    println!(
        "staged added={} modified={} removed={}",
        outcome.added, outcome.modified, outcome.removed
    );

    let index = repo.load_index()?;
    let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
    println!("tree_oid={}", tree_oid.to_hex());

    Ok(())
}
