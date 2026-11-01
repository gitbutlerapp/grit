//! Checkout must not wholesale-remove a top-level directory that the target tree still uses.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::PostOperationHooks;
use grit_test_support::git;

fn switch_existing(
    repo: &Repository,
    branch: &str,
    oid: ObjectId,
) -> grit_lib::porcelain::switch::SwitchOutcome {
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: branch.to_owned(),
                oid,
            },
            dry_run: false,
            hooks: PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("switch")
}

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
    tree_file_count_at(repo, &tree_oid_of_head(repo))
}

fn tree_file_count_at(repo: &std::path::Path, tree: &ObjectId) -> usize {
    git(
        repo,
        &["ls-tree", "-r", tree.to_hex().as_str(), "--name-only"],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .count()
}

fn assert_index_matches_tree(repo: &std::path::Path, tree: &ObjectId) {
    assert_eq!(
        index_file_count(repo),
        tree_file_count_at(repo, tree),
        "index entry count must match tree {}",
        tree.to_hex()
    );
    let diff_index = Command::new("git")
        .current_dir(repo)
        .args(["diff-index", "--quiet", "--cached", tree.to_hex().as_str()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git diff-index");
    assert!(
        diff_index.success(),
        "index must match tree {} (git diff-index --cached)",
        tree.to_hex()
    );
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

fn build_issue_2415_dir_to_file_layout(repo_root: &std::path::Path, count: usize) {
    for i in 1..=count {
        std::fs::create_dir_all(repo_root.join(format!("d{i}/sub"))).expect("mkdir");
        std::fs::write(repo_root.join(format!("d{i}/sub/f")), "x\n").expect("write");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "base"]);
}

fn build_issue_2415_dir_to_file_branch(repo_root: &std::path::Path, count: usize) {
    git(repo_root, &["switch", "-qc", "t2"]);
    for i in 1..=count {
        std::fs::remove_dir_all(repo_root.join(format!("d{i}"))).expect("rm dir");
        std::fs::write(repo_root.join(format!("d{i}")), "file\n").expect("write file");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "t2"]);
}

fn assert_clean_index_matches_head(repo: &std::path::Path) {
    assert_eq!(
        index_file_count(repo),
        head_tree_file_count(repo),
        "index entry count must match HEAD tree"
    );
    assert!(
        git_porcelain(repo).trim().is_empty(),
        "git status must be clean, got:\n{}",
        git_porcelain(repo)
    );
    git_fsck(repo);
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
fn switch_sixteen_dir_to_file_replacements_matches_git_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    build_issue_2415_dir_to_file_layout(tmp.path(), 16);
    build_issue_2415_dir_to_file_branch(tmp.path(), 16);
    let t2_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let t2_oid = ObjectId::from_hex(&t2_hex).expect("t2");
    git(tmp.path(), &["switch", "-q", "main"]);
    let main_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let main_oid = ObjectId::from_hex(&main_hex).expect("main");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    switch_existing(&grit_repo, "t2", t2_oid);
    assert_clean_index_matches_head(tmp.path());

    switch_existing(&grit_repo, "main", main_oid);
    assert_clean_index_matches_head(tmp.path());
}

#[test]
fn merge_fast_forward_twenty_dir_to_file_replacements_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    build_issue_2415_dir_to_file_layout(tmp.path(), 20);
    let main_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let main_oid = ObjectId::from_hex(&main_hex).expect("main");
    build_issue_2415_dir_to_file_branch(tmp.path(), 20);
    let t2_hex = git(tmp.path(), &["rev-parse", "HEAD"]).trim().to_owned();
    let t2_oid = ObjectId::from_hex(&t2_hex).expect("t2");
    git(tmp.path(), &["switch", "-q", "main"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let outcome = integrate_branch(
        &grit_repo,
        "refs/heads/main",
        main_oid,
        t2_oid,
        "t2",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("ff merge");
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { .. }
    ));
    assert_clean_index_matches_head(tmp.path());
}

/// 100 dirs × 20 files + 100 dangling symlinks → files on `t2` (>2048 tree diff entries).
#[cfg(unix)]
fn build_large_mixed_dir_to_file_fixture(
    repo_root: &std::path::Path,
) -> (ObjectId, ObjectId, ObjectId, ObjectId) {
    for i in 0..100 {
        std::fs::create_dir_all(repo_root.join(format!("dir{i}/sub"))).expect("mkdir");
        for j in 0..20 {
            std::fs::write(
                repo_root.join(format!("dir{i}/sub/f{j}")),
                format!("{i}-{j}\n"),
            )
            .expect("write");
        }
        std::os::unix::fs::symlink("nonexistent", repo_root.join(format!("dangling{i}")))
            .expect("symlink");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "base"]);
    let main_hex = git(repo_root, &["rev-parse", "HEAD"]).trim().to_owned();
    let main_oid = ObjectId::from_hex(&main_hex).expect("main");
    let from_tree = tree_oid_of_head(repo_root);

    git(repo_root, &["switch", "-qc", "t2"]);
    for i in 0..100 {
        std::fs::remove_dir_all(repo_root.join(format!("dir{i}"))).expect("rm dir");
        std::fs::write(repo_root.join(format!("dir{i}")), "file\n").expect("file");
        std::fs::remove_file(repo_root.join(format!("dangling{i}"))).expect("rm symlink");
        std::fs::write(repo_root.join(format!("dangling{i}")), "file\n").expect("symlink file");
    }
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "t2"]);
    let t2_hex = git(repo_root, &["rev-parse", "HEAD"]).trim().to_owned();
    let t2_oid = ObjectId::from_hex(&t2_hex).expect("t2");
    let to_tree = tree_oid_of_head(repo_root);

    git(repo_root, &["switch", "-q", "main"]);
    (main_oid, t2_oid, from_tree, to_tree)
}

#[cfg(unix)]
#[test]
fn switch_large_mixed_dir_to_file_and_symlink_replacements_both_ways() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    let (main_oid, t2_oid, _from_tree, _to_tree) =
        build_large_mixed_dir_to_file_fixture(tmp.path());

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    switch_existing(&grit_repo, "t2", t2_oid);
    assert_clean_index_matches_head(tmp.path());

    switch_existing(&grit_repo, "main", main_oid);
    assert_clean_index_matches_head(tmp.path());
}

#[cfg(unix)]
#[test]
fn hard_reset_large_mixed_dir_to_file_and_symlink_replacements() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    let (_main_oid, _t2_oid, from_tree, to_tree) =
        build_large_mixed_dir_to_file_fixture(tmp.path());
    git(tmp.path(), &["switch", "-q", "t2"]);

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    checkout_hard_to_tree_from_head_tree(
        &grit_repo,
        &to_tree,
        &from_tree,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("hard reset to main tree");
    assert_index_matches_tree(tmp.path(), &from_tree);
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
