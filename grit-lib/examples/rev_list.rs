//! Walk commits reachable from `HEAD` using [`rev_list::rev_list`] and [`RevListOptions`].
//!
//! Run: `cargo run -p grit-lib --example rev_list`

use grit_lib::objects::FileMode;
use grit_lib::progress::{CancelToken, NullProgress};

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_list::RevListOptions;

fn make_initial_commit(repo: &Repository) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    use grit_lib::index::{Index, IndexEntry};

    let blob_oid = repo.odb().write(ObjectKind::Blob, b"log line\n")?;
    let path = b"file.txt".to_vec();
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
        "root\n",
    );
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    repo.references().update(
        grit_lib::refs::store::RefNameRef::new("refs/heads/main")?,
        &oid,
    )?;
    Ok(oid)
}

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;
    let _root_commit = make_initial_commit(&repo)?;

    let opts = RevListOptions::default();
    let result = repo.revwalk().rev_list(
        &[String::from("HEAD")],
        &[],
        &opts,
        &mut NullProgress,
        &CancelToken::never(),
    )?;

    println!("commits from HEAD (oldest-first internal order may vary):");
    for oid in &result.commits {
        println!("  {oid}");
    }

    Ok(())
}
