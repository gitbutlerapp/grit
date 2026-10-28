//! Issue #2247: auto-stage must not respell tracked paths on case-folding volumes, and must
//! fold new untracked paths under existing index directory spellings.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn index_paths(root: &Path) -> Vec<String> {
    let index = Index::load(&root.join(".git/index")).expect("index");
    let mut paths: Vec<String> = index
        .entries()
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths
}

fn is_case_insensitive(dir: &Path) -> bool {
    let probe = dir.join(".grit_icase_probe");
    std::fs::write(&probe, b"").expect("write probe");
    let folded = dir.join(".GRIT_ICASE_PROBE").exists();
    std::fs::remove_file(&probe).expect("remove probe");
    folded
}

/// Repro A on Linux: both `Src/` and `src/` exist; unrelated commit must not rewrite either.
#[test]
fn ignorecase_unrelated_commit_preserves_dual_case_dirs_on_sensitive_volume() {
    if is_case_insensitive(std::env::temp_dir().as_path()) {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("Src")).expect("Src");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("Src/a.txt"), "a\n").expect("a");
    fs::write(root.join("src/b.txt"), "b\n").expect("b");
    for n in ["Src/a.txt", "src/b.txt"] {
        let oid = git(root, &["hash-object", "-w", n]).trim().to_string();
        git(
            root,
            &["update-index", "--add", "--cacheinfo", "100644", &oid, n],
        );
    }
    git(root, &["commit", "-qm", "coll"]);
    let before = index_paths(root);

    fs::write(root.join("unrelated.txt"), "edit\n").expect("unrelated");
    let repo = open(root);
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
    commit_worktree(
        &repo,
        &CommitRequest {
            message: "unrelated".to_owned(),
            ..CommitRequest::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");

    let after = index_paths(root);
    for path in &before {
        assert!(
            after.contains(path),
            "index path {path} must be unchanged, got {after:?}"
        );
    }
    let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let mut tree_paths: Vec<_> = tree.lines().collect();
    tree_paths.sort();
    assert!(tree_paths.contains(&"Src/a.txt"));
    assert!(tree_paths.contains(&"src/b.txt"));
    assert!(tree_paths.contains(&"unrelated.txt"));
}

/// Repro B on a case-folding volume: new files use index directory spelling; leaf aliases update in place.
#[test]
fn ignorecase_commit_keeps_index_spelling_on_folding_volume() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    if !is_case_insensitive(root) {
        return;
    }
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "core.ignorecase", "true"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);

    fs::create_dir_all(root.join("Dir")).expect("Dir");
    fs::write(root.join("Dir/f.txt"), "b\n").expect("f");
    fs::write(root.join("readme.txt"), "r\n").expect("readme");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);

    fs::rename(root.join("Dir"), root.join("dir")).expect("rename dir");
    fs::rename(root.join("readme.txt"), root.join("README.txt")).expect("rename readme");
    fs::write(root.join("dir/g.txt"), "g\n").expect("g");
    fs::write(root.join("README.txt"), "n\n").expect("readme edit");

    let repo = open(root);
    stage_for_commit(&repo, &mut NullProgress, &NeverCancel).expect("stage");
    commit_worktree(
        &repo,
        &CommitRequest {
            message: "two".to_owned(),
            ..CommitRequest::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit");

    let expected = ["Dir/f.txt", "Dir/g.txt", "readme.txt"];
    let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    let mut tree_paths: Vec<_> = tree.lines().collect();
    tree_paths.sort();
    assert_eq!(tree_paths, expected);
    git(root, &["fsck", "--strict"]);
}
