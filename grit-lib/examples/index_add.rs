//! Build an in-memory [`Index`], stage a path, and write `.git/index` via [`Repository::write_index`].
//!
//! Run: `cargo run -p grit-lib --example index_add`

use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::FileMode;
use grit_lib::objects::ObjectKind;
use grit_lib::repo::{InitOptions, Repository};

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;

    let blob_oid = repo.odb().write(ObjectKind::Blob, b"staged content\n")?;

    let path = b"notes.txt".to_vec();
    let entry = IndexEntry::new(path, blob_oid, FileMode::REGULAR);

    let mut index = Index::new();
    index.add_or_replace(entry);
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())?;

    let round_trip = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())?;
    println!("index entries: {}", round_trip.entries().len());
    let first = &round_trip.entries()[0];
    println!(
        "first path: {}, oid: {}",
        String::from_utf8_lossy(&first.path),
        first.oid
    );

    Ok(())
}
