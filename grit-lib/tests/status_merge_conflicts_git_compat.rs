//! Issue #940: merge conflicts should appear once in status with merge-in-progress state.

use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn merge_conflict_repo() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    std::fs::write(root.join("f"), "base\n").expect("write f");
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("f"), "side\n").expect("write side");
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f"), "main\n").expect("write main");
    git(root, &["commit", "-qam", "main"]);
    let merge = std::process::Command::new("git")
        .args(["merge", "side"])
        .current_dir(root)
        .output()
        .expect("git merge");
    assert_ne!(
        merge.status.code(),
        Some(0),
        "expected merge conflict: {}",
        String::from_utf8_lossy(&merge.stderr)
    );
    tmp
}

#[test]
fn status_lists_each_conflict_path_once_and_sets_merge_state() {
    let tmp = merge_conflict_repo();
    let root = tmp.path();
    let repo = Repository::discover(Some(root)).expect("open repo");
    let model = status(&repo, &StatusOptions::default(), &mut NullProgress).expect("status");

    assert!(
        model.state.merge_in_progress,
        "MERGE_HEAD should be detected"
    );
    assert_eq!(model.conflicts, vec!["f".to_owned()]);
    assert!(
        model.staged.iter().filter(|e| e.path() == "f").count() == 1,
        "staged: {:?}",
        model.staged
    );
    assert!(
        model
            .staged
            .iter()
            .all(|e| e.path() != "f" || e.status == grit_lib::diff::DiffStatus::Unmerged),
        "staged conflict row must be unmerged: {:?}",
        model.staged
    );
    assert!(
        model.unstaged.iter().all(|e| e.path() != "f"),
        "unstaged must not repeat conflict path: {:?}",
        model.unstaged
    );
    assert!(
        !model.untracked.iter().any(|p| p == "f"),
        "conflicted path must not be untracked: {:?}",
        model.untracked
    );
}
