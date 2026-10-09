//! Custom append-only object store wired through [`OdbBuilder::primary`].
//!
//! Writes a small commit graph in the KV store, walks history with [`rev_list`],
//! then exports objects as loose files for interoperability with system Git.

use std::env;
use std::path::PathBuf;

use grit_examples::packfile_kv;

fn main() -> grit_lib::error::Result<()> {
    let root = env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        dir.path().to_path_buf()
    });
    let log = packfile_kv::run_custom_object_store_demo(&root)?;
    for oid in log {
        println!("{oid}");
    }
    Ok(())
}
