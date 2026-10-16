//! Regression for campaign-scale UNTR deferral during tracked restage (task #1734).

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::stage_tracked::stage_tracked_modifications;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use std::fs;

use support::git;

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open grit repo")
}

fn assert_untr_warmed(index_bytes: &[u8]) {
    let idx = Index::parse(index_bytes).expect("parse index");
    let uc = idx
        .untracked_cache
        .as_ref()
        .expect("UNTR extension must exist after warm-up");
    let root = uc
        .root
        .as_ref()
        .expect("UNTR root must exist after warm-up");
    assert!(
        root.valid,
        "UNTR root must be valid before bulk defer replay"
    );
}

fn assert_untr_root_invalid(index_bytes: &[u8]) {
    let idx = Index::parse(index_bytes).expect("parse index");
    let uc = idx
        .untracked_cache
        .as_ref()
        .expect("UNTR extension must remain after bulk defer replay");
    let root = uc
        .root
        .as_ref()
        .expect("UNTR root entry must remain after bulk defer replay");
    assert!(
        !root.valid,
        "UNTR root must be invalid after bulk defer replay"
    );
}

fn warm_untr_cache(repo_dir: &std::path::Path) {
    git(repo_dir, &["config", "core.untrackedCache", "true"]);
    git(repo_dir, &["update-index", "--untracked-cache"]);
    git(repo_dir, &["status", "-s"]);
    assert_untr_warmed(&fs::read(repo_dir.join(".git/index")).expect("index"));
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
    warm_untr_cache(&repo_dir);

    for i in 0..20 {
        let p = repo_dir.join(format!("d{i:03}/f.txt"));
        let _ = fs::remove_file(p);
    }

    let grit = open_grit(&repo_dir);
    stage(&grit, &StageOptions::all(), &mut NullProgress).expect("grit add");

    let grit_index = fs::read(repo_dir.join(".git/index")).expect("grit index");
    assert_untr_root_invalid(&grit_index);

    git(&repo_dir, &["add", "-A"]);
    let git_index = fs::read(repo_dir.join(".git/index")).expect("git index");
    assert_untr_root_invalid(&git_index);
}

#[test]
fn stage_tracked_modifications_public_api_replays_untr_after_bulk_removal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    git(&repo_dir, &["init", "-q", "-b", "main", "."]);

    const ENTRIES: usize = 5_000;
    for i in 0..ENTRIES {
        let dir = repo_dir.join(format!("d{i:04}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("f.txt"), format!("{i}\n")).unwrap();
    }
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-q", "-m", "initial"]);
    warm_untr_cache(&repo_dir);

    for i in 0..500 {
        let p = repo_dir.join(format!("d{i:04}/f.txt"));
        let _ = fs::remove_file(p);
    }

    let grit = open_grit(&repo_dir);
    let wt = grit.work_tree().expect("work tree");
    let summary = stage_tracked_modifications(&grit, wt).expect("stage tracked");
    assert_eq!(summary.removed.len(), 500);
    assert!(summary.untracked_invalidation_deferred);

    let grit_index = fs::read(repo_dir.join(".git/index")).expect("grit index");
    assert_untr_root_invalid(&grit_index);
}
