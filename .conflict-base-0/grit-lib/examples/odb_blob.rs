//! Write a blob into the object database and read it back with [`Odb::write`] and [`Odb::read`].
//!
//! Run: `cargo run -p grit-lib --example odb_blob`

use grit_lib::objects::{Object, ObjectKind};
use grit_lib::repo::{InitOptions, Repository};

fn main() -> grit_lib::error::Result<()> {
    let root = tempfile::tempdir()?;
    let repo = Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )?;

    let payload = b"hello, object database\n";
    let oid = repo.odb().write(ObjectKind::Blob, payload)?;
    println!("stored blob: {oid}");

    let Object { kind, data } = repo.odb().read(&oid)?;
    assert_eq!(kind, ObjectKind::Blob);
    println!("round-trip: {}", String::from_utf8_lossy(&data));

    Ok(())
}
