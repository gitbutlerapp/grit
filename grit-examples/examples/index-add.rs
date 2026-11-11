// API docs: https://docs.rs/grit-lib/latest/grit_lib/index/struct.Index.html
use grit_lib::index::{Index, IndexEntry};
use grit_lib::objects::FileMode;
use grit_lib::objects::ObjectId;

fn main() -> grit_lib::error::Result<()> {
    let oid = ObjectId::from_hex("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391")?;
    let mut index = Index::new();
    let path = b"src/main.rs".to_vec();

    index.add_or_replace(IndexEntry::new(path, oid, FileMode::REGULAR));

    index.sort();
    println!("index version {}", index.version);
    Ok(())
}
