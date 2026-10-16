//! Init must not delete template files probed for NFD/NFC aliasing.

use std::fs;

use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

#[test]
fn init_preserves_template_file_at_probe_nfc_name() {
    let tmpl = TempDir::new().expect("tempdir");
    const NFC: &str = "\u{00e4}";
    fs::write(tmpl.path().join(NFC), b"template-payload").expect("write template");

    let root = TempDir::new().expect("worktree");
    Repository::init(
        root.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files)
            .with_template_dir(Some(tmpl.path().to_path_buf())),
    )
    .expect("init");

    let git_file = root.path().join(".git").join(NFC);
    assert!(
        git_file.is_file(),
        "template file must survive init filesystem probe"
    );
    assert_eq!(
        fs::read(&git_file).expect("read"),
        b"template-payload",
        "template bytes must be unchanged"
    );
}
