//! Init-time filesystem config probes (`core.precomposeunicode` / `core.ignorecase`).

use std::fs;

use grit_lib::environment::Environment;
use grit_lib::init_filesystem::{apply_init_filesystem_config, InitFilesystemConfigOptions};
use grit_lib::repo::init_repository;
use tempfile::TempDir;

#[test]
fn init_sets_precomposeunicode_when_options_force_probe() {
    let td = TempDir::new().expect("tempdir");
    init_repository(td.path(), false, "main", None, "files").expect("init");
    apply_init_filesystem_config(
        &td.path().join(".git"),
        InitFilesystemConfigOptions {
            force_precompose_probe: true,
            ..InitFilesystemConfigOptions::default()
        },
        &Environment::empty(),
    )
    .expect("apply");
    let config = fs::read_to_string(td.path().join(".git/config")).expect("config");
    assert!(
        config.contains("precomposeunicode = true"),
        "expected precomposeunicode in config:\n{config}"
    );
}
