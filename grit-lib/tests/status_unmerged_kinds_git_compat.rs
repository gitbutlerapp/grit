//! Unmerged conflict kinds vs system `git status --porcelain=v2` (t7060 / t7508 cases).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{conflict_kind_from_stages, status, ConflictKind, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;
use std::process::Command;
use tempfile::TempDir;

fn git_porcelain_v2_xy(root: &std::path::Path, path: &str) -> String {
    let out = Command::new("git")
        .args(["status", "--porcelain=v2"])
        .current_dir(root)
        .output()
        .expect("git status");
    assert!(
        out.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if !line.starts_with('u') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.last() == Some(&path) {
            return parts[1].to_owned();
        }
    }
    panic!("no porcelain v2 unmerged line for {path}:\n{text}");
}

fn xy_to_kind(xy: &str) -> ConflictKind {
    match xy {
        "UU" => ConflictKind::BothModified,
        "AA" => ConflictKind::BothAdded,
        "DD" => ConflictKind::BothDeleted,
        "AU" => ConflictKind::AddedByUs,
        "UA" => ConflictKind::AddedByThem,
        "DU" => ConflictKind::DeletedByUs,
        "UD" => ConflictKind::DeletedByThem,
        other => panic!("unknown xy {other}"),
    }
}

fn assert_kind_matches_git(root: &std::path::Path, path: &str) {
    let xy = git_porcelain_v2_xy(root, path);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    let detail = model
        .conflict_details
        .iter()
        .find(|c| c.path == path)
        .unwrap_or_else(|| panic!("missing conflict {path}: {:?}", model.conflict_details));
    assert_eq!(detail.kind, xy_to_kind(&xy), "git xy {xy}");
}

#[test]
fn git_merge_both_modified_is_uu() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    std::fs::write(root.join("f.txt"), "base\n").expect("write");
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("f.txt"), "side\n").expect("write");
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("f.txt"), "main\n").expect("write");
    git(root, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(root)
        .output()
        .expect("merge");
    assert_ne!(merge.status.code(), Some(0), "expected conflict");
    assert_kind_matches_git(root, "f.txt");
}

#[test]
fn git_merge_both_added_is_aa() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    std::fs::write(root.join("README"), "seed\n").expect("write");
    git(root, &["add", "README"]);
    git(root, &["commit", "-qm", "seed"]);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("both.txt"), "side\n").expect("write");
    git(root, &["add", "both.txt"]);
    git(root, &["commit", "-qm", "side add"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("both.txt"), "main\n").expect("write");
    git(root, &["add", "both.txt"]);
    git(root, &["commit", "-qm", "main add"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(root)
        .output()
        .expect("merge");
    assert_ne!(merge.status.code(), Some(0), "expected conflict");
    assert_kind_matches_git(root, "both.txt");
}

#[test]
fn git_merge_deleted_by_them_is_ud() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    std::fs::write(root.join("gone.txt"), "base\n").expect("write");
    git(root, &["add", "gone.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    git(root, &["rm", "gone.txt"]);
    git(root, &["commit", "-qm", "side rm"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("gone.txt"), "main\n").expect("write");
    git(root, &["add", "gone.txt"]);
    git(root, &["commit", "-qm", "main change"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(root)
        .output()
        .expect("merge");
    assert_ne!(merge.status.code(), Some(0), "expected conflict");
    assert_kind_matches_git(root, "gone.txt");
}

#[test]
fn git_merge_modify_delete_reports_du() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    std::fs::write(root.join("gone.txt"), "base\n").expect("write");
    git(root, &["add", "gone.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    std::fs::write(root.join("gone.txt"), "side\n").expect("write");
    git(root, &["add", "gone.txt"]);
    git(root, &["commit", "-qm", "side"]);
    git(root, &["checkout", "-q", "main"]);
    git(root, &["rm", "gone.txt"]);
    git(root, &["commit", "-qm", "main delete"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(root)
        .output()
        .expect("merge");
    assert_ne!(merge.status.code(), Some(0), "expected conflict");
    assert_eq!(git_porcelain_v2_xy(root, "gone.txt"), "DU");
    assert_kind_matches_git(root, "gone.txt");
}

#[test]
fn stage_masks_match_porcelain_xy_table() {
    assert_eq!(
        conflict_kind_from_stages(true, true, true),
        ConflictKind::BothModified
    );
    assert_eq!(xy_to_kind("UU"), ConflictKind::BothModified);
}
