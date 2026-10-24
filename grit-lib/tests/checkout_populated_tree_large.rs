//! Populated-tree checkout (empty index → full tree) on large flat repos.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::repo::Repository;

use support::{git, populate_flat_files};

#[test]
fn populated_tree_checkout_matches_git_for_many_files() {
    const COUNT: usize = 12_000;
    let root = tempfile::tempdir().expect("tempdir");
    git(root.path(), &["init", "-q", "-b", "main", "."]);
    populate_flat_files(root.path(), "file-with-a-longer-name-", COUNT);
    git(root.path(), &["add", "-A"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e.com",
            "commit",
            "-m",
            "init",
        ],
    );
    let tree_hex = git(root.path(), &["rev-parse", "HEAD^{tree}"]);
    let tree = tree_hex.trim().parse().expect("tree oid");

    let repo =
        Repository::discover(Some(root.path()), &RepositoryOptions::empty()).expect("open repo");
    let mut index = Index::new();
    repo.index_handle().write(&mut index).expect("clear index");
    for i in 0..COUNT {
        let path = root
            .path()
            .join(format!("file-with-a-longer-name-{i:04}.txt"));
        std::fs::remove_file(path).expect("remove worktree file");
    }

    checkout_between_trees(&repo, None, &tree).expect("repopulate tree");

    let status = git(root.path(), &["status", "--porcelain"]);
    assert!(
        status.trim().is_empty(),
        "git status should be clean after populated checkout, got:\n{status}"
    );
    let ls_count = git(root.path(), &["ls-files"]).lines().count();
    assert_eq!(ls_count, COUNT, "expected every file in index");
}
