//! Resolve a revision string such as `HEAD` or a full hex object id with [`rev_parse::resolve_revision`].
//!
//! Run: `cargo run -p grit-lib --example rev_parse`

use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::write_tree::write_tree_from_index;

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(root.path(), false, "main", None, "files")?;

    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
    let blob_oid = repo.odb.write(ObjectKind::Blob, b"x\n")?;
    let path = b"a".to_vec();
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
        flags: 1,
        flags_extended: None,
        path,
        base_index_pos: 0,
    };
    let mut index = Index::new();
    index.add_or_replace(entry);
    repo.write_index(&mut index)?;
    let index = repo.load_index()?;
    let tree_oid = write_tree_from_index(&repo.odb, &index, "")?;
    let commit = CommitData {
        tree: tree_oid,
        parents: Vec::new(),
        author: "Example <example@example.com> 1700000000 +0000".to_owned(),
        committer: "Example <example@example.com> 1700000000 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "r\n".to_owned(),
        raw_message: None,
        preserved_preamble: Vec::new(),
    };
    let oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &oid)?;

    let head = resolve_revision(&repo, "HEAD")?;
    let full = resolve_revision(&repo, &oid.to_hex())?;
    println!("HEAD resolves to {head}");
    println!("full hex resolves to {full}");
    assert_eq!(head, full);

    Ok(())
}
