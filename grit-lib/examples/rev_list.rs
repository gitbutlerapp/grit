//! Walk commits reachable from `HEAD` using [`rev_list::rev_list`] and [`RevListOptions`].
//!
//! Run: `cargo run -p grit-lib --example rev_list`

use grit_lib::objects::{serialize_commit, CommitData, ObjectKind};
use grit_lib::refs;
use grit_lib::repo::init_repository;
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, OutputMode, RevListOptions};
use grit_lib::write_tree::write_tree_from_index;

fn make_initial_commit(repo: &Repository) -> grit_lib::error::Result<grit_lib::objects::ObjectId> {
    use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};

    let blob_oid = repo.odb.write(ObjectKind::Blob, b"log line\n")?;
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

    let commit = CommitData {
        tree: tree_oid,
        parents: Vec::new(),
        author: "Example <example@example.com> 1700000000 +0000".to_owned(),
        committer: "Example <example@example.com> 1700000000 +0000".to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: "root\n".to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let oid = repo
        .odb
        .write(ObjectKind::Commit, &serialize_commit(&commit))?;
    refs::write_ref(&repo.git_dir, "refs/heads/main", &oid)?;
    Ok(oid)
}

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(
        root.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )?;
    let _root_commit = make_initial_commit(&repo)?;

    let mut opts = RevListOptions::default();
    opts.output_mode = OutputMode::OidOnly;
    let result = rev_list(&repo, &[String::from("HEAD")], &[], &opts)?;

    println!("commits from HEAD (oldest-first internal order may vary):");
    for oid in &result.commits {
        println!("  {oid}");
    }

    Ok(())
}
