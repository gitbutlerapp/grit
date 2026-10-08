//! Recursively walk a tree object with [`objects::parse_tree`] (a minimal tree walker).
//!
//! Run: `cargo run -p grit-lib --example walk_tree`

use grit_lib::index::MODE_TREE;
use grit_lib::objects::{parse_tree, serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;
use grit_lib::rev_parse::resolve_revision;
use grit_lib::write_tree::write_tree_from_index;

fn walk_tree(
    repo: &grit_lib::repo::Repository,
    tree_oid: grit_lib::objects::ObjectId,
    prefix: &str,
) -> grit_lib::error::Result<()> {
    let obj = repo.odb.read(&tree_oid)?;
    let entries = parse_tree(&obj.data)?;
    for e in entries {
        let name = String::from_utf8_lossy(&e.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if e.mode == MODE_TREE {
            println!("{path}/ (tree {})", e.oid);
            walk_tree(repo, e.oid, &path)?;
        } else {
            println!("{path} -> {}", e.oid);
        }
    }
    Ok(())
}

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(root.path(), false, "main", None, "files")?;

    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};

    let blob_a = repo.odb.write(ObjectKind::Blob, b"a\n")?;
    let blob_b = repo.odb.write(ObjectKind::Blob, b"b\n")?;

    let mut index = Index::new();
    for (rel, oid) in [
        (b"a.txt".as_slice(), blob_a),
        (b"sub/b.txt".as_slice(), blob_b),
    ] {
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
            oid,
            flags: (rel.len().min(0xfff)) as u16,
            flags_extended: None,
            path: rel.to_vec(),
            base_index_pos: 0,
        };
        index.add_or_replace(entry);
    }
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
        message: "tree walk\n".to_owned(),
        raw_message: None,
        preserved_preamble: Vec::new(),
    };
    let commit_oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &commit_oid)?;

    let head_commit = resolve_revision(&repo, "HEAD")?;
    let commit_obj = repo.odb.read(&head_commit)?;
    let parsed = grit_lib::objects::parse_commit(&commit_obj.data)?;
    println!("walking tree at {}", parsed.tree);
    walk_tree(&repo, parsed.tree, "")?;

    Ok(())
}
