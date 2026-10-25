//! Tracked restage (serial, and parallel in several bounded chunks) matches `git add -u`.
//!
//! More changed paths than one restage chunk holds, plus deletions, mode-only changes and
//! stat-only touches, staged with and without untracked-cache deferral.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::stage_tracked::{
    stage_tracked_modifications_with_thresholds, IndexStagingThresholds,
};
use grit_lib::repo::Repository;
use std::fs;
use std::path::Path;

use support::{assert_git_fsck_strict, git};

const DIRS: usize = 30;
const FILES_PER_DIR: usize = 100;
const CONTENT_CHANGES: usize = 2_600;
const MODE_CHANGES: usize = 5;
const TOUCHES: usize = 20;
const REMOVALS: usize = 10;

fn rel(i: usize) -> String {
    format!("d{:02}/f{:03}.txt", i / FILES_PER_DIR, i % FILES_PER_DIR)
}

fn seed(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main", "."]);
    git(repo, &["config", "core.untrackedCache", "true"]);
    for i in 0..DIRS * FILES_PER_DIR {
        let path = repo.join(rel(i));
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, format!("file {i}\n")).expect("seed file");
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "initial"]);
    git(repo, &["status", "-s"]);
}

fn clone(source: &Path, parent: &Path, name: &str) -> std::path::PathBuf {
    let src = source.to_str().expect("utf-8 path");
    git(parent, &["clone", "--local", "-q", src, name]);
    let dest = parent.join(name);
    git(&dest, &["config", "core.untrackedCache", "true"]);
    git(&dest, &["status", "-s"]);
    dest
}

fn mutate(repo: &Path) {
    let mut i = 0;
    for _ in 0..CONTENT_CHANGES {
        fs::write(repo.join(rel(i)), format!("changed {i}\n")).expect("modify");
        i += 1;
    }
    #[cfg(unix)]
    for _ in 0..MODE_CHANGES {
        use std::os::unix::fs::PermissionsExt;
        let path = repo.join(rel(i));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        i += 1;
    }
    #[cfg(not(unix))]
    {
        i += MODE_CHANGES;
    }
    for _ in 0..TOUCHES {
        let path = repo.join(rel(i));
        let body = fs::read(&path).expect("read");
        fs::remove_file(&path).expect("unlink");
        fs::write(&path, body).expect("rewrite same content");
        i += 1;
    }
    for _ in 0..REMOVALS {
        fs::remove_file(repo.join(rel(i))).expect("remove");
        i += 1;
    }
}

fn assert_matches_git_add_u(large_index_restage: usize) {
    assert_matches_git_add_u_with(IndexStagingThresholds {
        large_index_restage,
        parallel_tracked_restage: 64,
    });
}

fn assert_matches_git_add_u_with(thresholds: IndexStagingThresholds) {
    let large_index_restage = thresholds.large_index_restage;
    let tmp = tempfile::tempdir().expect("tempdir");
    let oracle = tmp.path().join("oracle");
    fs::create_dir_all(&oracle).expect("mkdir oracle");
    seed(&oracle);
    let grit_root = clone(&oracle, tmp.path(), "grit");

    mutate(&oracle);
    mutate(&grit_root);

    let repo = Repository::discover(Some(&grit_root), &RepositoryOptions::empty())
        .expect("open grit repo");
    let summary = stage_tracked_modifications_with_thresholds(
        &repo,
        repo.work_tree().expect("wt"),
        thresholds,
    )
    .expect("stage tracked");
    assert_eq!(summary.removed.len(), REMOVALS);
    assert!(
        summary.modified.len() >= CONTENT_CHANGES + MODE_CHANGES,
        "modified {}",
        summary.modified.len()
    );
    assert_eq!(
        summary.untracked_invalidation_deferred,
        DIRS * FILES_PER_DIR >= large_index_restage
    );

    git(&oracle, &["add", "-u"]);
    assert_eq!(
        git(&grit_root, &["ls-files", "-s"]),
        git(&oracle, &["ls-files", "-s"])
    );
    assert_eq!(
        git(&grit_root, &["write-tree"]),
        git(&oracle, &["write-tree"])
    );
    assert_eq!(git(&grit_root, &["diff", "--name-only"]), "");
    assert_eq!(
        git(&grit_root, &["status", "--porcelain"]),
        git(&oracle, &["status", "--porcelain"])
    );
    assert_git_fsck_strict(&grit_root);
}

#[test]
fn chunked_parallel_restage_matches_git_with_untracked_deferral() {
    assert_matches_git_add_u(64);
}

#[test]
fn chunked_parallel_restage_matches_git_without_untracked_deferral() {
    assert_matches_git_add_u(usize::MAX);
}

#[test]
fn serial_restage_matches_git() {
    assert_matches_git_add_u_with(IndexStagingThresholds {
        large_index_restage: usize::MAX,
        parallel_tracked_restage: usize::MAX,
    });
}
