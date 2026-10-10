use std::fs;
use std::process::Command;

use anyhow::{bail, Context, Result};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::init_repository;

#[test]
fn hash_object_writes_file_contents_as_blob() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let repo = init_repository(
        temp.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )?;
    let input_path = temp.path().join("hello.txt");
    let contents = b"hello from grit examples\n";
    fs::write(&input_path, contents)?;

    let output = Command::new(env!("CARGO_BIN_EXE_gritx-hash-object"))
        .arg(&input_path)
        .current_dir(temp.path())
        .output()
        .context("failed to run gritx-hash-object")?;

    if !output.status.success() {
        bail!(
            "gritx-hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout)?.trim().to_owned();
    let object_id = stdout.parse()?;
    let object = repo.odb.read(&object_id)?;

    assert_eq!(object.kind, ObjectKind::Blob);
    assert_eq!(object.data, contents);

    Ok(())
}
