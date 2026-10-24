//! Regression: staging many resolved literal file pathspecs touches only those index rows.

use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;

fn init_dense_repo(file_count: usize) -> (tempfile::TempDir, Repository, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    let wt = repo.work_tree().unwrap();
    let mut paths = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let rel = format!("leaf{i:04}/file.txt");
        fs::create_dir_all(wt.join(format!("leaf{i:04}"))).expect("mkdir");
        fs::write(wt.join(&rel), format!("v{i}\n")).expect("write");
        paths.push(rel);
    }
    let opts = StageOptions {
        mode: StageMode::All,
        ..StageOptions::default()
    };
    stage(&repo, &opts, &mut NullProgress).expect("initial add");
    (dir, repo, paths)
}

#[test]
fn stage_hundred_literal_paths_only_updates_those_files() {
    const TOTAL: usize = 2_000;
    const TOUCH: usize = 100;
    let (_dir, repo, paths) = init_dense_repo(TOTAL);
    let wt = repo.work_tree().unwrap();
    let subset: Vec<String> = paths
        .into_iter()
        .step_by(TOTAL / TOUCH)
        .take(TOUCH)
        .collect();
    for p in &subset {
        fs::write(wt.join(p), b"changed\n").expect("modify");
    }
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
}
