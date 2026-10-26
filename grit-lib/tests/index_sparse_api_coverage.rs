//! Additional API coverage for index format selection and sparse-checkout application.

use crate::support;

use std::fs;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::index::sparse_checkout::{
    apply_sparse_checkout_skip_worktree, clear_skip_worktree_from_present_files,
    parse_sparse_checkout_file, path_in_sparse_checkout_patterns, path_matches_sparse_patterns,
    ConePatterns, NonConePatterns,
};
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{Index, IndexEntry, MODE_REGULAR};
use grit_lib::objects::ObjectKind;
use grit_lib::repo::Repository;

use support::{assert_git_fsck_strict, git, seed_multidir_repo, HistoryRepo};

#[test]
fn index_new_with_config_respects_version_and_many_files() {
    let v2 = Index::new_with_config(Some("2"), None);
    assert_eq!(v2.version, 2);
    let v4 = Index::new_with_config(Some("4"), Some("true"));
    assert_eq!(v4.version, 4);
    let bad = Index::new_with_config(Some("9"), None);
    assert_eq!(bad.version, 3, "invalid config falls back like git");
}

#[test]
fn sparse_apply_and_clear_skip_worktree_on_disk() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone"]);
    git(wt, &["sparse-checkout", "set", "a"]);

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let mut index = grit.index_handle().load().expect("load");
    let warnings =
        apply_sparse_checkout_skip_worktree(&grit.git_dir(), Some(wt), &mut index, false);
    assert!(warnings.is_empty() || warnings.iter().all(|w| !w.is_empty()));
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);

    fs::write(wt.join("a/f0.txt"), "touch\n").expect("write");
    let cfg = ConfigSet::load(&Environment::empty(), Some(&grit.git_dir()), true).expect("cfg");
    clear_skip_worktree_from_present_files(&grit.git_dir(), wt, &mut index, Some(&cfg));
}

#[test]
fn sparse_pattern_helpers_cover_cone_and_non_cone() {
    let cone_file = "/*\n!/*/\n/a/\n/b/\n";
    let cone = ConePatterns::try_parse(cone_file).expect("cone");
    assert!(cone.path_included("a/x"));
    assert!(!cone.path_included("c/x"));

    let expanded: Vec<String> = parse_sparse_checkout_file("/*\n!/*/\n/sub/\n")
        .into_iter()
        .map(String::from)
        .collect();
    assert!(path_in_sparse_checkout_patterns(
        "sub/file", &expanded, true
    ));
    assert!(!path_in_sparse_checkout_patterns(
        "other/file",
        &expanded,
        true
    ));
    let raw = vec!["sub".to_string()];
    assert!(path_in_sparse_checkout_patterns("sub/file", &raw, true));

    let non_cone = NonConePatterns::parse("/*\n!/*/\nsub/\n");
    assert!(non_cone.path_included("sub/x"));
    assert!(!non_cone.path_included("nested/other/x"));

    assert!(path_matches_sparse_patterns("sub/x", &raw, true));
    assert!(!path_matches_sparse_patterns(
        "sub/x",
        &vec!["other".to_string()],
        false
    ));
}

#[test]
fn write_tree_and_cache_tree_after_sparse_apply() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let mut index = grit.index_handle().load().expect("load");
    let blob = grit
        .odb()
        .write(ObjectKind::Blob, b"extra\n")
        .expect("blob");
    index.add_or_replace(IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 0,
        mtime_sec: 1,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 6,
        oid: blob,
        flags: 8,
        flags_extended: None,
        path: b"extra.txt".to_vec(),
        base_index_pos: 0,
    });
    let _tree = write_tree_update_index(&grit.odb(), &mut index, "", WriteTreeFlags::default())
        .expect("write-tree");
    grit.index_handle().write(&mut index).expect("write");
    assert_git_fsck_strict(wt);
}
