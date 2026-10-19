//! Campaign-scale (>=80k index entries) UNTR defer replay vs system Git.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::stage_tracked::{
    stage_tracked_modifications_with_thresholds, IndexStagingThresholds,
};
use grit_lib::repo::Repository;
use std::fs;
use std::io::Write;
use std::path::Path;

use support::git;

const ENTRIES: usize = 81_000;
const REMOVE: usize = 15_000;
const LARGE_THRESHOLD: usize = 80_000;

fn open_grit(wt: &Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open grit repo")
}

fn untr_root_valid(index_bytes: &[u8]) -> bool {
    let idx = Index::parse(index_bytes).expect("parse index");
    idx.untracked_cache
        .as_ref()
        .and_then(|uc| uc.root.as_ref())
        .is_some_and(|r| r.valid)
}

fn seed_tracked_files(repo_dir: &Path, count: usize) {
    for batch_start in (0..count).step_by(500) {
        let batch_end = (batch_start + 500).min(count);
        for i in batch_start..batch_end {
            let path = repo_dir.join(format!("f{i:06}.txt"));
            let mut f = fs::File::create(path).expect("create file");
            f.write_all(b"x\n").expect("write file");
        }
    }
}

fn init_committed_repo(repo_dir: &Path) {
    fs::create_dir_all(repo_dir).unwrap();
    git(repo_dir, &["init", "-q", "-b", "main", "."]);
    seed_tracked_files(repo_dir, ENTRIES);
    git(repo_dir, &["add", "-A"]);
    git(repo_dir, &["commit", "-q", "-m", "initial"]);
    git(repo_dir, &["status", "-s"]);
}

/// Second oracle repo: hardlink objects via `git clone --local` (same disk as one full seed).
fn clone_committed_repo(source: &Path, dest: &Path) {
    if dest.exists() {
        fs::remove_dir_all(dest).expect("remove clone dest");
    }
    let parent = dest.parent().expect("clone dest parent");
    let source_str = source.to_str().expect("source path utf-8");
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .expect("clone dest name");
    git(parent, &["clone", "--local", "-q", source_str, name]);
}

fn remove_first_n_files(repo_dir: &Path, n: usize) {
    for i in 0..n {
        let _ = fs::remove_file(repo_dir.join(format!("f{i:06}.txt")));
    }
}

#[test]
fn bulk_untr_replay_when_index_drops_below_threshold_after_removals() {
    assert!(
        ENTRIES >= LARGE_THRESHOLD,
        "fixture must start at or above 80k entries"
    );

    let git_dir = tempfile::tempdir().expect("tempdir");
    init_committed_repo(git_dir.path());
    let grit_dir = tempfile::tempdir().expect("tempdir");
    clone_committed_repo(git_dir.path(), &grit_dir.path().join("repo"));
    let grit_root = grit_dir.path().join("repo");

    let index_before = Index::load(&grit_root.join(".git/index")).expect("load index");
    assert!(
        index_before.entries().len() >= LARGE_THRESHOLD,
        "index at start: {}",
        index_before.entries().len()
    );

    remove_first_n_files(&grit_root, REMOVE);
    remove_first_n_files(git_dir.path(), REMOVE);

    let grit = open_grit(&grit_root);
    let wt = grit.work_tree().expect("work tree");
    let campaign_thresholds = IndexStagingThresholds {
        large_index_restage: LARGE_THRESHOLD,
        parallel_tracked_restage: LARGE_THRESHOLD,
    };
    let summary = stage_tracked_modifications_with_thresholds(&grit, wt, campaign_thresholds)
        .expect("stage tracked");
    assert_eq!(summary.removed.len(), REMOVE);
    assert!(
        summary.untracked_invalidation_deferred,
        "defer must reflect entry count at start (>= {LARGE_THRESHOLD}), not after removals"
    );

    let grit_index = fs::read(grit_root.join(".git/index")).expect("grit index");
    let grit_parsed = Index::parse(&grit_index).expect("parse grit index");
    let final_count = grit_parsed.entries().len();
    assert_eq!(final_count, ENTRIES - REMOVE);
    assert!(
        final_count < LARGE_THRESHOLD,
        "final index {final_count} must be below bulk threshold {LARGE_THRESHOLD}"
    );
    assert!(
        !untr_root_valid(&grit_index),
        "grit must persist invalid UNTR root after bulk defer replay"
    );

    git(git_dir.path(), &["add", "-A"]);
    let git_index = fs::read(git_dir.path().join(".git/index")).expect("git index");
    assert!(
        !untr_root_valid(&git_index),
        "git add -A must invalidate UNTR after mass removal"
    );
}
