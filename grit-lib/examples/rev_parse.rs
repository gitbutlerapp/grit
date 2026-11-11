//! Resolve a revision string such as `HEAD` or a full hex object id with [`rev_parse::resolve_revision`].
//!
//! Run: `cargo run -p grit-lib --example rev_parse`

use grit_lib::index::write_tree::write_tree_from_index;
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

    use grit_lib::index::{Index, IndexEntry};
    let blob_oid = repo.odb().write(ObjectKind::Blob, b"x\n")?;
    let path = b"a".to_vec();
    let entry = IndexEntry::new(path, blob_oid, FileMode::REGULAR);
    let mut index = Index::new();
    index.add_or_replace(entry);
    repo.index()
        .write(&mut index, &grit_lib::index::IndexWriteOptions::default())?;
    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())?;
    let tree_oid = write_tree_from_index(repo.odb(), &index, "")?;
    let commit = CommitData::new(
        tree_oid,
        Vec::new(),
        "Example <example@example.com> 1700000000 +0000",
        "Example <example@example.com> 1700000000 +0000",
        "r\n",
    );
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    repo.references().update(
        grit_lib::refs::store::RefNameRef::new("refs/heads/main")?,
        &oid,
    )?;

    let head = repo
        .revwalk()
        .resolve("HEAD", &grit_lib::revwalk::ResolveOptions::default())?;
    let full = repo
        .revwalk()
        .resolve(&oid.to_hex(), &grit_lib::revwalk::ResolveOptions::default())?;
    println!("HEAD resolves to {head}");
    println!("full hex resolves to {full}");
    assert_eq!(head, full);

    Ok(())
}
