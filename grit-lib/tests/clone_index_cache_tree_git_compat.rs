//! Local clone must write a valid cache-tree (TREE index extension), matching system `git clone`.
//!
//! Without it, the first `grit status` / `grit diff` rebuild cache-tree in memory but never
//! persist it, leaving hot paths ~8× slower on wide trees (issue #1676).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{cache_tree_extension_valid, cache_tree_fully_valid};
use grit_lib::index::Index;
use grit_lib::remote::clone::{clone as clone_repo, CloneOptions};
use grit_lib::remote::fetch::NoProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

const FILE_COUNT: usize = 128;

fn init_wide_repo(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for i in 0..FILE_COUNT {
        let dir = root.join(format!("d{:02}", i / 32));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("f{i:04}.txt")), format!("{i}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "initial"]);
}

fn head_tree_oid(repo_root: &std::path::Path) -> grit_lib::objects::ObjectId {
    let hex = git(repo_root, &["rev-parse", "HEAD^{tree}"]);
    grit_lib::objects::ObjectId::from_hex(hex.trim()).expect("tree oid")
}

#[test]
fn grit_clone_index_has_cache_tree_matching_head() {
    let upstream = tempfile::tempdir().expect("upstream");
    init_wide_repo(upstream.path());

    let grit_dest = tempfile::tempdir().expect("grit dest");
    clone_repo(
        CloneOptions::new(
            upstream.path().to_string_lossy(),
            grit_dest.path().to_path_buf(),
        ),
        &mut NoProgress,
        None,
    )
    .expect("grit clone");

    let repo = Repository::open(
        &grit_dest.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(grit_dest.path()),
    )
    .expect("open grit clone");
    let index = Index::load(&repo.index_handle().path()).expect("load index");
    let head_tree = head_tree_oid(grit_dest.path());

    assert!(
        index.cache_tree.is_some(),
        "grit clone must persist TREE cache-tree extension (issue #1676)"
    );
    assert_eq!(
        index.cache_tree_root.as_ref(),
        Some(&head_tree),
        "cache-tree root must match HEAD tree"
    );
    assert!(cache_tree_extension_valid(index.cache_tree.as_ref()));
    assert!(cache_tree_fully_valid(
        repo.odb(),
        index.cache_tree.as_ref()
    ));
}

#[test]
fn populated_checkout_writes_cache_tree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    init_wide_repo(tmp.path());
    let tree = head_tree_oid(tmp.path());

    let repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open repo");

    let mut index = Index::new();
    repo.index_handle().write(&mut index).expect("clear index");

    grit_lib::porcelain::checkout::checkout_between_trees(&repo, None, &tree)
        .expect("populated checkout");

    let index = Index::load(&repo.index_handle().path()).expect("load index");
    assert!(
        index.cache_tree.is_some(),
        "populated checkout must write cache-tree"
    );
    assert_eq!(index.cache_tree_root.as_ref(), Some(&tree));
}
