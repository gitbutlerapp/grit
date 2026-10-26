//! Regression for campaign-scale UNTR deferral during tracked restage (task #1734).

use crate::support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::stage_tracked::{
    stage_tracked_modifications_with_thresholds, IndexStagingThresholds,
};
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

fn small_bulk_thresholds() -> IndexStagingThresholds {
    IndexStagingThresholds {
        large_index_restage: 64,
        parallel_tracked_restage: 64,
    }
}

fn stage_opts_with_small_bulk() -> StageOptions {
    StageOptions {
        mode: StageMode::All,
        executable_hint: Default::default(),
        staging_thresholds: Some(small_bulk_thresholds()),
        ..StageOptions::default()
    }
}

#[test]
fn bulk_deferred_untr_replay_after_mass_removal() {
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
    git(&repo_dir, &["status", "-s"]);

    for i in 0..20 {
        let p = repo_dir.join(format!("d{i:03}/f.txt"));
        let _ = fs::remove_file(p);
    }

    let grit = open_grit(&repo_dir);
    stage(&grit, &stage_opts_with_small_bulk(), &mut NullProgress).expect("grit add");

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

#[test]
fn stage_tracked_modifications_public_api_replays_untr_after_bulk_removal() {
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
    git(&repo_dir, &["status", "-s"]);

    for i in 0..20 {
        let p = repo_dir.join(format!("d{i:03}/f.txt"));
        let _ = fs::remove_file(p);
    }

    let grit = open_grit(&repo_dir);
    let wt = grit.work_tree().expect("work tree");
    let summary = stage_tracked_modifications_with_thresholds(&grit, wt, small_bulk_thresholds())
        .expect("stage tracked");
    assert_eq!(summary.removed.len(), 20);
    assert!(summary.untracked_invalidation_deferred);

    let grit_index = fs::read(repo_dir.join(".git/index")).expect("grit index");
    assert!(
        !untr_root_valid(&grit_index),
        "public stage_tracked_modifications must persist invalid UNTR after bulk defer replay"
    );
}
