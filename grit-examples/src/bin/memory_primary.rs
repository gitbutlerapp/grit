//! Open a repository whose primary object store is an in-memory [`MemoryStore`].
//!
//! Writes go to memory; the on-disk `objects/` tree stays untouched.

use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::odb::OdbBuilder;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::{InitOptions, Repository, RepositoryBuilder};

fn main() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    Repository::init(dir.path(), &InitOptions::default())?;
    let git_dir = dir.path().join(".git");

    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let repo = RepositoryBuilder::new(RepositoryOptions::empty())
        .odb_builder(
            OdbBuilder::files(git_dir.join("objects"))
                .primary(store.clone())
                .alternates(false),
        )
        .open(&git_dir, Some(dir.path()))?;

    let blob = repo.odb().write(ObjectKind::Blob, b"hello")?;
    println!("blob={blob}");
    println!("in_memory={}", store.contains(&blob)?);
    println!("on_disk={}", repo.odb().object_path(&blob).exists());
    Ok(())
}
