//! Stage paths in the index, build a tree with [`write_tree::write_tree_from_index`], create a
//! commit with [`objects::serialize_commit`], and point `refs/heads/main` at it with [`refs::write_ref`].
//!
//! Run: `cargo run -p grit-lib --example commit_tree`

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::FileMode;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;

    let blob_oid = repo
        .odb()
        .write(ObjectKind::Blob, b"hello from grit-lib examples\n")?;
    let path = b"README".to_vec();
    let entry = IndexEntry::new(path, blob_oid, FileMode::REGULAR);

    let mut index = Index::new();
    index.add_or_replace(entry);
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())?;

    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())?;
    let tree_oid = write_tree_from_index(repo.odb(), &index, "")?;
    println!("tree: {tree_oid}");

    let commit = CommitData::new(
        tree_oid,
        Vec::new(),
        "Example <example@example.com> 1700000000 +0000",
        "Example <example@example.com> 1700000000 +0000",
        "initial example commit\n",
    );
    let raw = serialize_commit(&commit);
    let commit_oid = repo.odb().write(ObjectKind::Commit, &raw)?;
    println!("commit: {commit_oid}");

    repo.references().update(
        grit_lib::refs::store::RefNameRef::new("refs/heads/main")?,
        &commit_oid,
    )?;
    println!("updated refs/heads/main -> {commit_oid}");

    Ok(())
}
