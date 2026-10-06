//! Regression tests for `submodule init` and repository-controlled `update` policy.
//!
//! Issue #896: init must not copy command-form `update = !…` from `.gitmodules` into
//! local config when the submodule URL is already registered.

mod common;

use crate::common::*;
use std::fs;
use std::path::{Path, PathBuf};

const FILE_ALLOW: &[(&str, &str)] = &[("protocol.file.allow", "always")];

/// Minimal superproject with one checked-out submodule named `submodule`.
fn setup_super_with_submodule(tag: &str) -> PathBuf {
    let root = unique_tmp("submodule-init", tag);
    let submodule = root.join("submodule");
    fs::create_dir_all(&submodule).expect("mkdir submodule");

    git_cmd(&["init", "-q", "-b", "main", "."])
        .in_dir(&submodule)
        .suc();
    write_file(&submodule, "file", "content\n");
    git_cmd(&["add", "file"]).in_dir(&submodule).suc();
    git_cmd(&["commit", "-q", "-m", "submodule"])
        .in_dir(&submodule)
        .suc();

    let super_dir = root.join("super");
    fs::create_dir_all(&super_dir).expect("mkdir super");
    git_cmd(&["init", "-q", "-b", "main", "."])
        .in_dir(&super_dir)
        .suc();
    for (k, v) in FILE_ALLOW {
        grit_cmd(&["config", k, v]).in_dir(&super_dir).suc();
    }
    grit_cmd(&["submodule", "add", "../submodule", "submodule"])
        .in_dir(&super_dir)
        .suc();
    grit_cmd(&["submodule", "init", "submodule"])
        .in_dir(&super_dir)
        .suc();

    super_dir
}

/// Register a second gitlink `submodule1` pointing at the same commit as `submodule`.
fn add_extra_gitlink(super_dir: &Path) {
    let sha = git_cmd(&["ls-files", "-s", "submodule"])
        .in_dir(super_dir)
        .suc()
        .stdout;
    let oid = sha
        .split_whitespace()
        .nth(1)
        .expect("gitlink oid from ls-files -s");
    fs::create_dir_all(super_dir.join("submodule1")).expect("mkdir submodule1");
    git_cmd(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000",
        oid,
        "submodule1",
    ])
    .in_dir(super_dir)
    .suc();
    git_cmd(&[
        "config",
        "-f",
        ".gitmodules",
        "submodule.submodule1.path",
        "submodule1",
    ])
    .in_dir(super_dir)
    .suc();
    git_cmd(&[
        "config",
        "-f",
        ".gitmodules",
        "submodule.submodule1.url",
        "../submodule",
    ])
    .in_dir(super_dir)
    .suc();
}

fn local_config_value(super_dir: &Path, key: &str) -> Option<String> {
    let out = grit_cmd(&["config", "--get", key]).in_dir(super_dir).exec();
    if out.status == Some(1) {
        return None;
    }
    assert!(
        out.ok(),
        "config --get {key}: {}",
        out.dump("grit config --get")
    );
    let v = out.stdout.trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

#[test]
fn init_rejects_command_update_when_url_already_in_local_config() {
    let super_dir = setup_super_with_submodule("url-already");
    add_extra_gitlink(&super_dir);

    git_cmd(&["config", "submodule.submodule1.url", "../submodule"])
        .in_dir(&super_dir)
        .suc();
    grit_cmd(&[
        "config",
        "-f",
        ".gitmodules",
        "submodule.submodule1.update",
        "!false",
    ])
    .in_dir(&super_dir)
    .suc();

    let _ = grit_cmd(&["submodule", "init", "submodule1"])
        .in_dir(&super_dir)
        .with_status(1);

    assert_eq!(
        local_config_value(&super_dir, "submodule.submodule1.update"),
        None,
        "command update must not be persisted in local config"
    );
}

#[test]
fn init_does_not_copy_command_update_on_first_registration() {
    let super_dir = setup_super_with_submodule("fresh-url");
    add_extra_gitlink(&super_dir);

    grit_cmd(&[
        "config",
        "-f",
        ".gitmodules",
        "submodule.submodule1.update",
        "!false",
    ])
    .in_dir(&super_dir)
    .suc();

    let _ = grit_cmd(&["submodule", "init", "submodule1"])
        .in_dir(&super_dir)
        .with_status(1);

    assert_eq!(
        local_config_value(&super_dir, "submodule.submodule1.update"),
        None
    );
}

#[test]
fn init_does_not_overwrite_trusted_local_update() {
    let super_dir = setup_super_with_submodule("keep-local");
    add_extra_gitlink(&super_dir);

    git_cmd(&["config", "submodule.submodule1.url", "../submodule"])
        .in_dir(&super_dir)
        .suc();
    git_cmd(&["config", "submodule.submodule1.update", "merge"])
        .in_dir(&super_dir)
        .suc();
    grit_cmd(&[
        "config",
        "-f",
        ".gitmodules",
        "submodule.submodule1.update",
        "rebase",
    ])
    .in_dir(&super_dir)
    .suc();

    grit_cmd(&["submodule", "init", "submodule1"])
        .in_dir(&super_dir)
        .suc();

    assert_eq!(
        local_config_value(&super_dir, "submodule.submodule1.update").as_deref(),
        Some("merge"),
        "existing local update policy must be preserved"
    );
}
