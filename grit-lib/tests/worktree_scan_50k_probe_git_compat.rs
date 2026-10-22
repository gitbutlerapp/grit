//! Issue #1518: directory-grouped scans on ~50k dense-leaf paths should not double-stat every file.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::stage_tracked::stage_tracked_modifications;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::{reset_worktree_metadata_probe, worktree_metadata_probe_count};
use grit_test_support::git;

const LEAF_DIRS: usize = 1_000;
const FILES_PER_LEAF: usize = 50;
const TRACKED: usize = LEAF_DIRS * FILES_PER_LEAF;

fn dense_leaf_prefix(leaf: usize) -> String {
    let depth = 5 + (leaf % 7);
    let mut path = String::new();
    for level in 0..depth {
        path.push_str(&format!(
            "p{}/q{}/r{}/s{}/n{}/",
            leaf % 10,
            level % 10,
            level % 8,
            level % 6,
            level % 5
        ));
    }
    path
}

fn build_fixture(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    for leaf in 0..LEAF_DIRS {
        let prefix = dense_leaf_prefix(leaf);
        let leaf_dir = format!("{prefix}leaf{leaf:04}");
        let abs_leaf = root.join(&leaf_dir);
        std::fs::create_dir_all(&abs_leaf).expect("mkdir");
        for f in 0..FILES_PER_LEAF {
            let path = abs_leaf.join(format!("f{f}-file.txt"));
            std::fs::write(path, format!("leaf={leaf} f={f}\n")).expect("write");
        }
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

#[test]
fn noop_tracked_scan_metadata_probe_budget_dense_leaf_50k() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let repo = open_repo(root);

    let _probe = reset_worktree_metadata_probe(root);
    let summary = stage_tracked_modifications(&repo, root).expect("tracked scan");
    assert_eq!(summary.change_count(), 0, "clean tree");
    let meta = worktree_metadata_probe_count();
    assert!(
        meta <= TRACKED + LEAF_DIRS + 64,
        "expected ~one metadata probe per tracked file, got {meta} (tracked {TRACKED})"
    );
}

#[test]
fn noop_add_metadata_probe_budget_dense_leaf_50k() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_fixture(root);
    let repo = open_repo(root);

    let _probe = reset_worktree_metadata_probe(root);
    let outcome = stage(&repo, &StageOptions::default(), &mut NullProgress).expect("add");
    assert_eq!(outcome.total(), 0);
    let meta = worktree_metadata_probe_count();
    assert!(
        meta <= TRACKED + LEAF_DIRS + 128,
        "no-op add should not double-stat every path (metadata {meta}, tracked {TRACKED})"
    );
}
