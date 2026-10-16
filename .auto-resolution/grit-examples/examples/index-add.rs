// API docs: https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::ObjectId;

fn main() -> grit_lib::error::Result<()> {
    let oid = ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")?;
    let mut index = Index::new();
    let path = b"src/main.rs".to_vec();

    index.add_or_replace(IndexEntry {
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
        flags: path.len() as u16,
        flags_extended: None,
        path,
        base_index_pos: 0,
    });

    index.sort();
    println!("index version {}", index.version);
    Ok(())
}
