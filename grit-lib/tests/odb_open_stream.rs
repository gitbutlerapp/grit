//! `Odb::open_stream` error propagation (corrupt local loose, alternates fallback).

use grit_lib::error::Error;
use grit_lib::objects::ObjectKind;
use grit_lib::odb::Odb;
use std::fs;
use tempfile::TempDir;

#[test]
fn open_stream_corrupt_local_loose_returns_error_not_none() {
    let root = TempDir::new().expect("tempdir");
    let objects = root.path().join("objects");
    let odb = Odb::new(&objects);
    let oid = odb
        .write(ObjectKind::Blob, b"payload")
        .expect("write loose");
    let path = odb.object_path(&oid);
    {
        use std::io::Write;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("writable");
        }
        let mut file = fs::File::create(&path).expect("open for corrupt");
        file.write_all(b"not-zlib").expect("corrupt bytes");
    }

    let result = odb.open_stream(&oid);
    assert!(matches!(
        result,
        Err(Error::Zlib(_)) | Err(Error::CorruptObject(_))
    ));
}
