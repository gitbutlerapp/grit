//! Build an in-memory [`Index`], stage a path, and write `.git/index` via [`Repository::write_index`].
//!
//! Run: `cargo run -p grit-lib --example index_add`

use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::init_repository;

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = init_repository(
        root.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )?;

    let blob_oid = repo.odb.write(ObjectKind::Blob, b"staged content\n")?;

    let path = b"notes.txt".to_vec();
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

    let round_trip = repo.load_index()?;
    println!("index entries: {}", round_trip.entries().len());
    let first = &round_trip.entries()[0];
    println!(
        "first path: {}, oid: {}",
        String::from_utf8_lossy(&first.path),
        first.oid
    );

    Ok(())
}
