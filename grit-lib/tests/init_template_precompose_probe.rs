//! Init must not delete template files probed for NFD/NFC aliasing.

use std::fs;

use grit_lib::repo::init_repository;
use tempfile::TempDir;

#[test]
fn init_preserves_template_file_at_probe_nfc_name() {
    let tmpl = TempDir::new().expect("tempdir");
    const NFC: &str = "\u{00e4}";
    fs::write(tmpl.path().join(NFC), b"template-payload").expect("write template");

    let root = TempDir::new().expect("worktree");
    init_repository(root.path(), false, "main", Some(tmpl.path()), "files").expect("init");

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
