//! Exercise index.rs APIs against git-produced repositories (coverage + compatibility).

mod support;

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::index::{
    entry_from_metadata, format_index_lock_blocked_detail, index_entries_stat_equivalent,
    munge_index_size, normalize_mode, worktree_path_from_index_rel, Index, IndexEntry,
    StatMatchPolicy, MODE_REGULAR,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_lib::revwalk::rev_parse;

use support::{git, seed_multidir_repo, HistoryRepo};

#[test]
fn index_stat_helpers_and_normalize_mode() {
    assert_eq!(munge_index_size(0), 0);
    assert_eq!(munge_index_size(0x1_0000_0000), 0x8000_0000);
    assert_eq!(normalize_mode(0o100755), 0o100755);
    assert_eq!(normalize_mode(0o120000), 0o120000);

    let a = IndexEntry {
        ctime_sec: 1,
        ctime_nsec: 2,
        mtime_sec: 3,
        mtime_nsec: 4,
        dev: 0,
        ino: 0,
        mode: MODE_REGULAR,
        uid: 0,
        gid: 0,
        size: 10,
        oid: ObjectId::zero(),
        flags: 1,
        flags_extended: None,
        path: b"p".to_vec(),
        base_index_pos: 0,
    };
    let mut b = a.clone();
    assert!(index_entries_stat_equivalent(
        &a,
        &b,
        StatMatchPolicy::default()
    ));
    b.mtime_sec = 4;
    assert!(!index_entries_stat_equivalent(
        &a,
        &b,
        StatMatchPolicy::default()
    ));

    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("f"), b"x").expect("write");
    let meta = fs::symlink_metadata(tmp.path().join("f")).expect("meta");
    let _ = entry_from_metadata(&meta, b"f", ObjectId::zero(), MODE_REGULAR);
    let wt = tmp.path();
    assert!(worktree_path_from_index_rel(wt, b"a/b").ends_with("a/b"));
}

#[test]
fn dump_cache_tree_and_format_dump() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let index = grit.index_handle().load().expect("load");
    assert!(index.cache_tree.is_some());
    let _ = index.format_cache_tree_dump();
    let dump = index.dump_cache_tree(&grit.odb()).expect("dump");
    assert!(!dump.is_empty());
}

#[test]
fn overlay_tree_on_index_from_head() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    fs::write(wt.join("a/f0.txt"), "dirty\n").expect("modify");
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let mut index = grit.index_handle().load().expect("load");
    index
        .overlay_tree_on_index(&grit, "HEAD", b"")
        .expect("overlay");
    assert!(index.entries().iter().any(|e| e.overlay_tree_skip_output()));
}

#[test]
fn load_expand_sparse_optional_and_collapse_roundtrip() {
    let repo = HistoryRepo::init();
    seed_multidir_repo(&repo);
    let wt = repo.worktree();
    git(wt, &["sparse-checkout", "init", "--cone"]);
    git(wt, &["sparse-checkout", "set", "a"]);
    git(wt, &["read-tree", "-mu", "HEAD"]);
    git(wt, &["config", "index.sparse", "true"]);
    git(wt, &["add", "-A"]);
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let head_commit = rev_parse::resolve_revision(&grit, "HEAD").expect("head");
    let head_obj = grit.odb().read(&head_commit).expect("commit");
    let head = grit_lib::objects::parse_commit(&head_obj.data)
        .expect("parse")
        .tree;
    let patterns = vec!["/*".to_string(), "!/*/".to_string(), "/a/".to_string()];
    let mut index =
        Index::load_expand_sparse_optional(&grit.index_handle().path(), &grit.odb()).expect("load");
    index
        .try_collapse_sparse_directories(&grit.odb(), &head, &patterns, true, true)
        .expect("collapse");
    if index.has_sparse_directory_placeholders() {
        let expanded =
            Index::load_expand_sparse(&grit.index_handle().path(), &grit.odb()).expect("expand");
        assert!(!expanded.has_sparse_directory_placeholders());
    }
}

#[test]
fn index_lock_blocked_detail_mentions_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("index");
    let detail = format_index_lock_blocked_detail(&path);
    assert!(detail.contains("index"));
}

#[test]
fn git_index_v4_long_paths_parse_and_rewrite() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path();
    git(wt, &["init", "-q", "-b", "main"]);
    let long_dir = "d/".to_string() + &"nested/".repeat(40);
    fs::create_dir_all(wt.join(&long_dir)).expect("mkdir");
    for i in 0..20 {
        fs::write(wt.join(format!("{long_dir}f{i}.txt")), format!("{i}\n")).expect("write");
    }
    Command::new("git")
        .current_dir(wt)
        .args(["add", "-A"])
        .env("GIT_INDEX_VERSION", "4")
        .status()
        .expect("git add");
    let bytes = fs::read(wt.join(".git/index")).expect("read index");
    let mut parsed = Index::parse(&bytes).expect("parse v4");
    assert_eq!(parsed.version, 4);
    parsed.write(&wt.join("out-index")).expect("rewrite");
    let reload = Index::load(&wt.join("out-index")).expect("reload");
    assert_eq!(reload.entries().len(), parsed.entries().len());
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let mut idx = reload;
    write_tree_update_index(&grit.odb(), &mut idx, "", WriteTreeFlags::default()).expect("tree");
}

#[test]
fn new_with_config_honors_many_files_and_version() {
    let idx = Index::new_with_config(Some("3"), Some("true"));
    assert_eq!(idx.version, 3);
}
