//! SQLite object store wired through [`OdbBuilder::primary`].
//!
//! Writes a small commit graph in SQLite, walks history with [`rev_list`],
//! then exports objects as loose files for interoperability with system Git.

use std::env;
use std::path::PathBuf;

use grit_examples::sqlite_odb;

fn main() -> grit_lib::error::Result<()> {
    let (root, _tmpdir) = match env::args().nth(1) {
        Some(path) => (PathBuf::from(path), None),
        None => {
            let dir = tempfile::tempdir().map_err(grit_lib::error::Error::Io)?;
            let root = dir.path().to_path_buf();
            (root, Some(dir))
        }
    };
    let log = sqlite_odb::run_sqlite_object_store_demo(&root)?;
    for oid in log {
        println!("{oid}");
    }
    Ok(())
}
