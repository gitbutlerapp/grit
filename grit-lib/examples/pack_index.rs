//! Inspect on-disk pack indexes under `objects/pack/` using [`pack::read_local_pack_indexes`].
//!
//! Repositories without packed objects report an empty list; after `git gc` (or fetch) you will
//! see `.pack` / `.idx` pairs listed here.
//!
//! Run: `cargo run -p grit-lib --example pack_index`

use grit_lib::pack;
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

    let objects_dir = repo.git_dir.join("objects");
    let indexes = pack::read_local_pack_indexes(&objects_dir)?;
    if indexes.is_empty() {
        println!("no pack indexes yet under {}", objects_dir.display());
        println!("(clone or run garbage collection to create .pack/.idx files)");
        return Ok(());
    }

    for idx in &indexes {
        println!("pack: {}", idx.pack_path.display());
        println!("  index entries: {}", idx.len());
        if let Some(e) = idx.iter().next() {
            println!(
                "  first oid: {} @ offset {}",
                pack::oid_bytes_to_hex(e.oid()),
                e.offset()
            );
        }
    }

    // Optional: parse a specific `.idx` file if you have a path.
    let pack_dir = objects_dir.join("pack");
    if pack_dir.is_dir() {
        for e in std::fs::read_dir(&pack_dir).map_err(grit_lib::error::Error::Io)? {
            let e = e.map_err(grit_lib::error::Error::Io)?;
            let p = e.path();
            if p.extension().is_some_and(|x| x == "idx") {
                let parsed = pack::read_pack_index(&p)?;
                println!("parsed {}: {} objects", p.display(), parsed.len());
                break;
            }
        }
    }

    Ok(())
}
