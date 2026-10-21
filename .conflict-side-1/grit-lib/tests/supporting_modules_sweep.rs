//! Exercise supporting-module APIs for line coverage (crlf, gitmodules, submodule, state).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::gitmodules::gitdir::{submodule_modules_git_dir, validate_submodule_path};
use grit_lib::config::gitmodules::{
    check_submodule_name, check_submodule_url, validate_gitmodules_blob_line,
};
use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::refs::branch_tracking::shorten_tracking_ref;
use grit_lib::repo::Repository;
use grit_lib::state::{detect_in_progress, repo_state, resolve_head};
use grit_lib::worktree::crlf::{
    convert_to_git, convert_to_worktree_eager, ConversionConfig, FileAttrs,
};
use std::path::Path;
use tempfile::TempDir;

#[test]
fn crlf_roundtrip_lf_crlf_matches_git_normalization() {
    let f = ConfigFile::parse(
        Path::new("cfg"),
        "[core]\n\tautocrlf = true\n",
        ConfigScope::Local,
    )
    .expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&f);
    let cfg = ConversionConfig::from_config(&set);
    let attrs = FileAttrs::default();
    let git_bytes = convert_to_git(b"line\r\n", "f.txt", &cfg, &attrs).expect("to git");
    assert_eq!(git_bytes, b"line\n");
    let wt = convert_to_worktree_eager(&git_bytes, "f.txt", &cfg, &attrs, None, None, None)
        .expect("to wt");
    assert_eq!(wt, b"line\r\n");
}

#[test]
fn gitmodules_and_submodule_gitdir_helpers() {
    assert!(check_submodule_name("valid/name"));
    assert!(check_submodule_url("https://example.com/x.git"));
    assert!(validate_gitmodules_blob_line(b"[submodule \"s\"]\n").is_none());
    let dir = TempDir::new().expect("tempdir");
    let git_dir = dir.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    let modules = submodule_modules_git_dir(&git_dir, "sub/path");
    assert!(modules.to_string_lossy().contains("modules"));
    validate_submodule_path(dir.path(), "sub").expect("validate path");
}

#[test]
fn state_detect_and_repo_state_on_fresh_repo() {
    let dir = TempDir::new().expect("tempdir");
    grit_lib::repo::Repository::init(dir.path(), &grit_lib::repo::InitOptions::default())
        .expect("init");
    let git_dir = dir.path().join(".git");
    assert!(detect_in_progress(&git_dir).is_empty());
    let _head = resolve_head(&git_dir).expect("head");
    let _repo = Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
    let _rs = repo_state(&git_dir, false).expect("repo_state");
    assert_eq!(
        shorten_tracking_ref("refs/remotes/origin/main"),
        "origin/main"
    );
}
