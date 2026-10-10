//! Init-time filesystem config probes (`core.precomposeunicode` / `core.ignorecase`).

use std::fs;

use grit_lib::environment::Environment;
use grit_lib::init_filesystem::{apply_init_filesystem_config, InitFilesystemConfigOptions};
use grit_lib::repo::init_repository;
use tempfile::TempDir;

#[test]
fn init_sets_precomposeunicode_when_options_force_probe() {
    let td = TempDir::new().expect("tempdir");
    init_repository(
        td.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
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

#[test]
fn second_init_does_not_overwrite_local_config() {
    let td = TempDir::new().expect("tempdir");
    init_repository(
        td.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let config_path = td.path().join(".git/config");
    let mut config = fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[custom]\n\tmarker = kept\n");
    fs::write(&config_path, &config).expect("write config");

    init_repository(
        td.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("reinit");

    let after = fs::read_to_string(&config_path).expect("config after");
    assert!(
        after.contains("marker = kept"),
        "local config override must survive reinit:\n{after}"
    );
}
