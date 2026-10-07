//! Init-time filesystem config probes (`core.precomposeunicode` / `core.ignorecase`).

use std::fs;

use grit_lib::repo::init_repository;
use tempfile::TempDir;

#[test]
fn init_sets_precomposeunicode_when_test_env_forces_probe() {
    std::env::set_var("GIT_TEST_UTF8_NFD_TO_NFC", "1");
    let td = TempDir::new().expect("tempdir");
    init_repository(td.path(), false, "main", None, "files").expect("init");
    std::env::remove_var("GIT_TEST_UTF8_NFD_TO_NFC");
    let config = fs::read_to_string(td.path().join(".git/config")).expect("config");
    assert!(
        config.contains("precomposeunicode = true"),
        "expected precomposeunicode in config:\n{config}"
    );
}
