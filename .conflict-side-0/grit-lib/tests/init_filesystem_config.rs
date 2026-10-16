//! Init-time filesystem config probes (`core.precomposeunicode` / `core.ignorecase`).

use std::fs;

use grit_lib::config::environment::Environment;
use grit_lib::config::init_filesystem::{
    apply_init_filesystem_config, InitFilesystemConfigOptions,
};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

#[test]
fn init_sets_precomposeunicode_when_options_force_probe() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(
        td.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
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
    Repository::init(
        td.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("init");
    let config_path = td.path().join(".git/config");
    let mut config = fs::read_to_string(&config_path).expect("config");
    config.push_str("\n[custom]\n\tmarker = kept\n");
    fs::write(&config_path, &config).expect("write config");

    Repository::init(
        td.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(grit_lib::RefStorageFormat::Files),
    )
    .expect("reinit");

    let after = fs::read_to_string(&config_path).expect("config after");
    assert!(
        after.contains("marker = kept"),
        "local config override must survive reinit:\n{after}"
    );
}
