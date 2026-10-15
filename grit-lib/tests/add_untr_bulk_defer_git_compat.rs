//! Regression for campaign-scale UNTR deferral during tracked restage (task #1734).

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use std::fs;

use support::git;

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open grit repo")
}

fn untr_root_valid(index_bytes: &[u8]) -> bool {
    let idx = Index::parse(index_bytes).expect("parse index");
    idx.untracked_cache
        .as_ref()
        .and_then(|uc| uc.root.as_ref())
        .is_some_and(|r| r.valid)
}

#[test]
fn bulk_deferred_untr_replay_after_mass_removal() {
    // Lower bulk threshold so the regression runs quickly in CI.
    unsafe { std::env::set_var("GRIT_TEST_BULK_THRESHOLD", "64") };

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    git(&repo_dir, &["init", "-q", "-b", "main", "."]);

    const ENTRIES: usize = 80;
    for i in 0..ENTRIES {
        let dir = repo_dir.join(format!("d{i:03}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), format!("{i}\n")).unwrap();
    }
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-q", "-m", "initial"]);
    // Warm UNTR on the index.
    git(&repo_dir, &["status", "-s"]);

    for i in 0..20 {
        let p = repo_dir.join(format!("d{i:03}/f.txt"));
        let _ = fs::remove_file(p);
    }

    let grit = open_grit(&repo_dir);
    let opts = StageOptions {
        pathspecs: Vec::new(),
        pathspec_sources: Vec::new(),
        mode: StageMode::All,
    };
    stage(&grit, &opts, &mut NullProgress).expect("grit add");

    let grit_index = fs::read(repo_dir.join(".git/index")).expect("grit index");
    assert!(
        !untr_root_valid(&grit_index),
        "grit must invalidate UNTR after bulk defer replay when entries were removed"
    );

    git(&repo_dir, &["add", "-A"]);
    let git_index = fs::read(repo_dir.join(".git/index")).expect("git index");
    assert!(
        !untr_root_valid(&git_index),
        "git reference: UNTR root should be invalid after mass removal add"
    );
}
