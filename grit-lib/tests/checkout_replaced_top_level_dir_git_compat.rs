//! Checkout must not wholesale-remove a top-level directory that the target tree still uses.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::PostOperationHooks;
use grit_test_support::git;

fn git_porcelain(repo: &std::path::Path) -> String {
    git(repo, &["status", "--porcelain"])
}

fn git_fsck(repo: &std::path::Path) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tree_oid_of_head(repo: &std::path::Path) -> ObjectId {
    let hex = git(repo, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("tree oid")
}

fn index_file_count(repo: &std::path::Path) -> usize {
    git(repo, &["ls-files"])
        .lines()
        .filter(|l| !l.is_empty())
        .count()
}

fn head_tree_file_count(repo: &std::path::Path) -> usize {
    git(repo, &["ls-tree", "-r", "HEAD", "--name-only"])
        .lines()
        .filter(|l| !l.is_empty())
        .count()
}

fn build_issue_2336_master_layout(repo_root: &std::path::Path) {
    std::fs::create_dir_all(repo_root.join("src/bin/cargo")).expect("mkdir");
    for i in 1..=5 {
        std::fs::write(
            repo_root.join(format!("src/bin/cargo/c{i}.rs")),
            format!("{i}\n"),
        )
        .expect("write");
    }
    std::fs::create_dir_all(repo_root.join("crates")).expect("mkdir");
    for i in 1..=30 {
        std::fs::write(repo_root.join(format!("crates/f{i}.txt")), format!("{i}\n"))
            .expect("write");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "master"]);
}

fn build_issue_2336_new_layout(repo_root: &std::path::Path) {
    git(repo_root, &["rm", "-rqf", "src", "crates"]);
    std::fs::create_dir_all(repo_root.join("src/bin")).expect("mkdir");
    for i in 1..=20 {
        std::fs::write(repo_root.join(format!("src/bin/b{i}.rs")), format!("{i}\n"))
            .expect("write");
    }
    std::fs::write(repo_root.join("README"), b"x\n").expect("readme");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "new"]);
}

fn build_large_chunk_replaced_src(repo_root: &std::path::Path) -> (ObjectId, ObjectId, ObjectId) {
    std::fs::create_dir_all(repo_root.join("src/deep")).expect("mkdir");
    for i in 0..2100 {
        std::fs::write(
            repo_root.join(format!("src/deep/f{i:04}.txt")),
            format!("old {i}\n"),
        )
        .expect("write old");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "old"]);
    let old_hex = git(repo_root, &["rev-parse", "HEAD"]).trim().to_owned();
    let from_tree = tree_oid_of_head(repo_root);

    git(repo_root, &["rm", "-rqf", "src"]);
    std::fs::create_dir_all(repo_root.join("src/new")).expect("mkdir");
    for i in 0..2100 {
        std::fs::write(
            repo_root.join(format!("src/new/f{i:04}.txt")),
            format!("new {i}\n"),
        )
        .expect("write new");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "new"]);
    let to_tree = tree_oid_of_head(repo_root);
    let new_commit_hex = git(repo_root, &["rev-parse", "HEAD"]).trim().to_owned();
    let new_commit = ObjectId::from_hex(&new_commit_hex).expect("new commit");

    git(repo_root, &["reset", "--hard", &old_hex]);
    (from_tree, to_tree, new_commit)
}

#[test]
fn checkout_replaced_src_tree_matches_git_after_issue_2336_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "master", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    build_issue_2336_master_layout(tmp.path());
    let from_tree = tree_oid_of_head(tmp.path());
    build_issue_2336_new_layout(tmp.path());
    let to_commit_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let to_tree = tree_oid_of_head(tmp.path());
    git(tmp.path(), &["reset", "--hard", "master"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    checkout_between_trees(&grit_repo, Some(&from_tree), &to_tree).expect("checkout");
    let to_commit = ObjectId::from_hex(&to_commit_hex).expect("commit");
    refs::write_ref(grit_repo.git_dir(), "HEAD", &to_commit).expect("head");

    assert_eq!(
        index_file_count(tmp.path()),
        head_tree_file_count(tmp.path()),
        "index entry count must match HEAD tree"
    );
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status must be clean, got:\n{}",
        git_porcelain(tmp.path())
    );
    git_fsck(tmp.path());
}

#[test]
fn switch_detach_replaced_src_tree_matches_git_after_issue_2336_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "master", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    build_issue_2336_master_layout(tmp.path());
    build_issue_2336_new_layout(tmp.path());
    let new_commit_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    git(tmp.path(), &["checkout", "-q", "-f", "master"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let new_commit = ObjectId::from_hex(&new_commit_hex).expect("commit");
    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Detached {
                commit: new_commit,
                reflog_to_label: "new".to_owned(),
            },
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("switch detach");

    assert_eq!(
        index_file_count(tmp.path()),
        head_tree_file_count(tmp.path()),
        "index entry count must match HEAD tree"
    );
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status must be clean, got:\n{}",
        git_porcelain(tmp.path())
    );
    git_fsck(tmp.path());
}

#[test]
fn checkout_replaced_src_with_chunked_wholesale_deletes_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    let (from_tree, to_tree, new_commit) = build_large_chunk_replaced_src(tmp.path());

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    checkout_between_trees(&grit_repo, Some(&from_tree), &to_tree).expect("checkout");
    refs::write_ref(grit_repo.git_dir(), "HEAD", &new_commit).expect("head");

    assert_eq!(
        index_file_count(tmp.path()),
        head_tree_file_count(tmp.path()),
        "index entry count must match HEAD tree after chunked checkout"
    );
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status must be clean after chunked checkout, got:\n{}",
        git_porcelain(tmp.path())
    );
    git_fsck(tmp.path());
}
