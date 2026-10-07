//! Write a blob, tree, and commit through [`grit_lib::odb::Odb`], then read them back.
//!
//! Source for the library guide "Objects" page (included in the docs site).

use grit_lib::objects::{
    parse_commit, parse_tree, serialize_commit, serialize_tree, CommitData, ObjectKind, TreeEntry,
};
use grit_lib::repo::{init_repository, Repository};
use std::path::{Path, PathBuf};

fn write_demo_objects(
    repo: &Repository,
) -> Result<
    (
        grit_lib::objects::ObjectId,
        grit_lib::objects::ObjectId,
        grit_lib::objects::ObjectId,
    ),
    grit_lib::error::Error,
> {
    let blob_data = b"hello from the library guide\n";
    let blob_oid = repo.odb.write(ObjectKind::Blob, blob_data)?;

    let tree_entries = vec![TreeEntry {
        mode: 0o100644,
        name: b"README".to_vec(),
        oid: blob_oid,
    }];
    let tree_oid = repo
        .odb
        .write(ObjectKind::Tree, &serialize_tree(&tree_entries))?;

    let commit = CommitData {
        tree: tree_oid,
        parents: Vec::new(),
        author: "Ada Lovelace <ada@example.com> 0 +0000".to_owned(),
        committer: "Ada Lovelace <ada@example.com> 0 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "Library guide objects example\n".to_owned(),
        raw_message: None,
    };
    let commit_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;

    let tree_obj = repo.odb.read(&tree_oid)?;
    let entries = parse_tree(&tree_obj.data)?;
    assert_eq!(entries[0].oid, blob_oid);

    let commit_obj = repo.odb.read(&commit_oid)?;
    let parsed = parse_commit(&commit_obj.data)?;
    assert_eq!(parsed.tree, tree_oid);

    Ok((blob_oid, tree_oid, commit_oid))
}

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
    let repo = if let Some(root) = std::env::args().nth(1).map(PathBuf::from) {
        open_repo(&root)?
    } else {
        let temp = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
        init_repository(temp.path(), false, "main", None, "files")?;
        open_repo(temp.path())?
    };

    let (blob_oid, tree_oid, commit_oid) = write_demo_objects(&repo)?;
    println!("{commit_oid}");
    println!("blob={blob_oid} tree={tree_oid}");
    Ok(())
}
