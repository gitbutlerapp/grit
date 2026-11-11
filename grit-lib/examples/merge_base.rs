//! Compute a merge base with [`merge_base::merge_bases_first_vs_rest`] after creating two branch tips.
//!
//! Run: `cargo run -p grit-lib --example merge_base`

use grit_lib::index::write_tree::write_tree_from_index;
use grit_lib::objects::FileMode;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};

fn commit_tree(
    repo: &grit_lib::repo::Repository,
    parent: Option<grit_lib::objects::ObjectId>,
    message: &str,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    use grit_lib::index::{Index, IndexEntry};

    let blob_oid = repo.odb().write(ObjectKind::Blob, b"content\n")?;
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

    let mut parents = Vec::new();
    if let Some(p) = parent {
        parents.push(p);
    }
    let commit = CommitData::new(
        tree_oid,
        parents,
        "Example <example@example.com> 1700000000 +0000",
        "Example <example@example.com> 1700000000 +0000",
        format!("{message}\n"),
    );
    repo.odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
}

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let (repo, _) = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Files),
    )?;

    let base = commit_tree(&repo, None, "common root")?;
    repo.references().update(
        grit_lib::refs::store::RefNameRef::new("refs/heads/main")?,
        &base,
    )?;

    let feature_tip = commit_tree(&repo, Some(base), "feature work")?;
    repo.references().update(
        grit_lib::refs::store::RefNameRef::new("refs/heads/feature")?,
        &feature_tip,
    )?;

    let main_tip = base;
    let bases = repo.revwalk().merge_bases(
        &[feature_tip, main_tip],
        grit_lib::revwalk::MergeBaseMode::Best,
    )?;
    println!("merge base(s) of feature and main: {bases:?}");
    assert!(bases.contains(&main_tip));

    Ok(())
}
