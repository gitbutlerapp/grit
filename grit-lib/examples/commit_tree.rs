//! Stage paths in the index, build a tree with [`write_tree::write_tree_from_index`], create a
//! commit with [`objects::serialize_commit`], and point `refs/heads/main` at it with [`refs::write_ref`].
//!
//! Run: `cargo run -p grit-lib --example commit_tree`

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;
use grit_lib::write_tree::write_tree_from_index;

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(root.path(), false, "main", None, "files")?;

    let blob_oid = repo
        .odb
        .write(ObjectKind::Blob, b"hello from grit-lib examples\n")?;
    let path = b"README".to_vec();
    let entry = IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 0,
        oid: blob_oid,
        flags: (path.len().min(0xfff)) as u16,
        flags_extended: None,
        path,
        base_index_pos: 0,
    };

    let mut index = Index::new();
    index.add_or_replace(entry);
    repo.write_index(&mut index)?;

    let index = repo.load_index()?;
    let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
    println!("tree: {tree_oid}");

    let commit = CommitData {
        tree: tree_oid,
        parents: Vec::new(),
        author: "Example <example@example.com> 1700000000 +0000".to_owned(),
        committer: "Example <example@example.com> 1700000000 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "initial example commit\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let raw = serialize_commit(&commit);
    let commit_oid = repo.odb.write(ObjectKind::Commit, &raw)?;
    println!("commit: {commit_oid}");

    refs::write_ref(&repo.git_dir, "refs/heads/main", &commit_oid)?;
    println!("updated refs/heads/main -> {commit_oid}");

    Ok(())
}
