//! Compute a merge base with [`merge_base::merge_bases_first_vs_rest`] after creating two branch tips.
//!
//! Run: `cargo run -p grit-lib --example merge_base`

use grit_lib::merge_base::merge_bases_first_vs_rest;
use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;
use grit_lib::write_tree::write_tree_from_index;

fn commit_tree(
    repo: &grit_lib::repo::Repository,
    parent: Option<grit_lib::objects::ObjectId>,
    message: &str,
) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};

    let blob_oid = repo.odb.write(ObjectKind::Blob, b"content\n")?;
    let path = b"file.txt".to_vec();
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

    let mut parents = Vec::new();
    if let Some(p) = parent {
        parents.push(p);
    }
    let commit = CommitData {
        tree: tree_oid,
        parents,
        author: "Example <example@example.com> 1700000000 +0000".to_owned(),
        committer: "Example <example@example.com> 1700000000 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{message}\n"),
        raw_message: None,
        preserved_preamble: Vec::new(),
    };
    repo.odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))
}

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(root.path(), false, "main", None, "files")?;

    let base = commit_tree(&repo, None, "common root")?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &base)?;

    let feature_tip = commit_tree(&repo, Some(base), "feature work")?;
    refs::write_ref(&repo.git_dir, "refs/heads/feature", &feature_tip)?;

    let main_tip = base;
    let bases = merge_bases_first_vs_rest(&repo, feature_tip, &[main_tip])?;
    println!("merge base(s) of feature and main: {bases:?}");
    assert!(bases.contains(&main_tip));

    Ok(())
}
