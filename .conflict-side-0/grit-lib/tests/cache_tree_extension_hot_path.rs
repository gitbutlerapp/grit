//! Cache-tree extension structural validity on a git-produced index (hot-path trust).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::write_tree::{cache_tree_extension_valid, cache_tree_fully_valid};
use grit_lib::index::Index;
use grit_lib::repo::Repository;
use grit_test_support::git;

#[test]
fn git_committed_index_cache_tree_extension_valid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    git(repo_root, &["init", "-q", "-b", "main", "."]);
    git(repo_root, &["config", "user.email", "t@example.com"]);
    git(repo_root, &["config", "user.name", "Test"]);

    const DIRS: usize = 20;
    const FILES_PER_DIR: usize = 50;
    for d in 0..DIRS {
        let dir = repo_root.join(format!("d{d:02}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..FILES_PER_DIR {
            std::fs::write(dir.join(format!("f{f:03}.txt")), format!("{d}/{f}\n")).expect("write");
        }
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "initial"]);

    let grit_repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open repo");
    let index = Index::load(&grit_repo.index_handle().path()).expect("load git index");
    assert!(
        index.cache_tree.is_some(),
        "git commit must write a TREE extension"
    );
    assert!(
        cache_tree_extension_valid(index.cache_tree.as_ref()),
        "git-written cache-tree must pass structural hot-path validation"
    );
    assert!(
        cache_tree_fully_valid(grit_repo.odb(), index.cache_tree.as_ref()),
        "git-written cache-tree must remain ODB-valid"
    );
}
