//! Commit staging on deep trees: untracked-cache fast path and no double refresh (issue #1016).

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::stage_for_commit;
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_repo(root: &std::path::Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

/// Shallow wide layout: many leaf directories with a few tracked files each.
fn write_deep_tree(root: &std::path::Path, branches: usize, depth: usize, files_per_leaf: usize) {
    for b in 0..branches {
        for d in 0..depth {
            for f in 0..files_per_leaf {
                let rel = format!("b{b:02}/d{d:02}/f{f}.txt");
                let path = root.join(&rel);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("mkdir");
                }
                fs::write(&path, format!("{rel}\n")).expect("write");
            }
        }
    }
}

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

#[test]
fn stage_for_commit_stages_single_edit_on_deep_tree_without_new_untracked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 8, 8, 3);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let repo = open_repo(root);
    status(&repo, &StatusOptions::default(), &mut NullProgress).expect("warm untracked cache");

    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n# edit\n").expect("attrs");

    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage for commit");
    assert_eq!(outcome.modified, 1, "expected one staged path");
    assert_eq!(
        outcome.added, 0,
        "clean tree should not stage untracked paths"
    );
}

#[test]
fn create_commit_nothing_to_commit_without_second_stat_refresh() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 4, 4, 2);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage clean tree");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    let err = create_commit(
        &repo,
        &CommitRequest {
            message: "noop".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: true,
            skip_pre_commit_tree_match: false,
        },
        &mut NullProgress,
    )
    .expect_err("clean tree");
    assert!(
        matches!(err, grit_lib::error::Error::NothingToCommit),
        "expected NothingToCommit, got {err:?}"
    );
}

/// Exercises UNTR auto-threshold (4096+ index entries) with a wider tree than the smoke layout.
#[test]
fn stage_for_commit_untracked_cache_eligible_tree_stages_one_edit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 25, 25, 7);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    assert!(
        git(root, &["ls-files"]).lines().count() >= 4096,
        "fixture should exceed UNTR auto-cache entry threshold"
    );

    let repo = open_repo(root);
    status(&repo, &StatusOptions::default(), &mut NullProgress).expect("warm untracked cache");
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n# edit\n").expect("edit");

    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage");
    assert_eq!(outcome.modified, 1);
    assert_eq!(outcome.added, 0);
}

#[test]
fn grit_commit_on_deep_tree_leaves_clean_tree_for_git() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo(root);
    write_deep_tree(root, 6, 6, 3);
    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n").expect("attrs");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n# x\n").expect("edit");

    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage");
    let ident = "Test <t@example.com> 1 +0000".to_string();
    create_commit(
        &repo,
        &CommitRequest {
            message: "t".into(),
            author: ident.clone(),
            committer: ident,
            allow_empty: false,
            sign_override: None,
            amend: false,
            assume_worktree_synced: true,
            skip_pre_commit_tree_match: false,
        },
        &mut NullProgress,
    )
    .expect("grit commit");

    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git should see a clean tree after grit commit, got:\n{porcelain}"
    );
    let out = Command::new("git")
        .args(["fsck", "--no-progress"])
        .current_dir(root)
        .output()
        .expect("git fsck");
    assert!(out.status.success(), "git fsck after grit commit");
}
