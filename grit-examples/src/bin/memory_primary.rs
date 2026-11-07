//! Create a repository whose primary object store is an in-memory [`MemoryStore`].
//!
//! Writes go to memory; [`Repository::init`] creates no `objects/` directory on disk.

use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};

fn main() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
    let (repo, _) = Repository::init(
        dir.path(),
        &InitOptions::new()
            .with_repository_options(RepositoryOptions::empty().with_object_store(store.clone())),
    )?;

    let blob = repo.odb().write(ObjectKind::Blob, b"hello")?;
    println!("blob={blob}");
    println!("in_memory={}", store.contains(&blob)?);
    println!("objects_dir={}", repo.git_dir().join("objects").exists());
    Ok(())
}
