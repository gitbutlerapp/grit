//! Read the index, stage paths, and write a tree from staged entries.
//!
//! Source for the library guide "Index" page (included in the docs site).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
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
    let mut opts = RepositoryOptions::empty();
    if let Some(wt) = work_tree {
        opts = opts.with_work_tree(wt);
    }
    Repository::open(&git_dir, &opts)
}

fn main() -> Result<(), grit_lib::error::Error> {
    let mut temp_guard = None;
    let repo = if let Some(root) = std::env::args().nth(1).map(PathBuf::from) {
        open_repo(&root)?
    } else {
        let temp = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
        Repository::init(
            temp.path(),
            &InitOptions::new()
                .with_initial_branch("main")
                .with_ref_storage(grit_lib::RefStorageFormat::Files),
        )?;
        let path = temp.path().join("hello.txt");
        fs::write(&path, b"staged from the library guide\n").map_err(grit_lib::error::Error::Io)?;
        let opened = open_repo(temp.path())?;
        temp_guard = Some(temp);
        opened
    };
    let _keep = temp_guard;

    let index = repo.index_handle().load()?;
    println!("index_entries={}", index.entries().len());

    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress)?;
    println!(
        "staged added={} modified={} removed={}",
        outcome.added.len(),
        outcome.modified.len(),
        outcome.removed.len()
    );

    let index = repo.index_handle().load()?;
    let tree_oid = write_tree_from_index(repo.odb(), &index, "")?;
    println!("tree_oid={}", tree_oid.to_hex());

    Ok(())
}
