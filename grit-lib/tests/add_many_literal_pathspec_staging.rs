//! Regression: staging many resolved literal file pathspecs touches only those index rows.

use grit_lib::index::Index;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::prelude::RepositoryOptions;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::fs;

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::discover(Some(root), &RepositoryOptions::empty()).expect("discover repo")
}

fn init_dense_repo(file_count: usize) -> (tempfile::TempDir, Repository, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let mut paths = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let rel = format!("leaf{i:04}/file.txt");
        fs::create_dir_all(root.join(format!("leaf{i:04}"))).expect("mkdir");
        fs::write(root.join(&rel), format!("v{i}\n")).expect("write");
        paths.push(rel);
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    let repo = open_repo(root);
    (dir, repo, paths)
}

fn index_oid(index: &Index, path: &str) -> ObjectId {
    index
        .get(path.as_bytes(), 0)
        .unwrap_or_else(|| panic!("missing index entry for {path}"))
        .oid
}

#[test]
fn stage_hundred_literal_paths_only_updates_those_files() {
    const TOTAL: usize = 2_000;
    const TOUCH: usize = 100;
    const STEP: usize = TOTAL / TOUCH;
    let (_dir, repo, paths) = init_dense_repo(TOTAL);
    let wt = repo.work_tree().unwrap();
    let subset: Vec<String> = (0..TOUCH)
        .map(|k| format!("leaf{:04}/file.txt", k * STEP))
        .collect();
    assert_eq!(subset.len(), TOUCH);
    for p in &paths {
        if !subset.contains(p) {
            fs::write(wt.join(p), b"dirty-not-in-pathspec\n").expect("dirty untouched");
        }
    }
    for p in &subset {
        fs::write(wt.join(p), b"changed\n").expect("modify");
    }
    let untouched = "leaf0001/file.txt";
    assert!(
        !subset.contains(&untouched.to_owned()),
        "fixture must include a dirty path outside the pathspec list"
    );
    let index_path = repo.index_handle().path();
    let oid_before = index_oid(&Index::load(&index_path).expect("load index"), untouched);

    let opts = StageOptions {
        pathspecs: subset.clone(),
        pathspec_sources: subset.clone(),
        mode: StageMode::All,
        ..StageOptions::default()
    };
    let outcome = stage(&repo, &opts, &mut NullProgress).expect("stage subset");
    assert_eq!(
        outcome.modified.len(),
        TOUCH,
        "expected one modified entry per literal pathspec"
    );
    for p in &subset {
        assert!(
            outcome.modified.iter().any(|m| m == p),
            "missing staged path {p}"
        );
    }

    let index_after = Index::load(&index_path).expect("load index after");
    assert_eq!(
        index_oid(&index_after, untouched),
        oid_before,
        "paths outside the pathspec list must not be restaged"
    );
}
