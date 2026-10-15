//! After `switch_repository`, system `git` sees a clean index and worktree.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_porcelain(repo: &std::path::Path) -> String {
    git(repo, &["status", "--porcelain"])
}

fn git_diff_index_head(repo: &std::path::Path) -> String {
    git(repo, &["diff-index", "HEAD"])
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

fn populate_initial(repo_root: &std::path::Path) {
    for i in 0..500 {
        let dir = repo_root.join(format!("d{:03}", i % 20));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("f{i:04}.txt")), format!("seed {i}\n")).expect("write");
    }
    std::fs::write(repo_root.join("swap.txt"), b"file-before\n").expect("swap");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "initial"]);
}

fn build_target_branch(repo_root: &std::path::Path) {
    git(repo_root, &["checkout", "-q", "-b", "target"]);
    for i in (0..500).step_by(17) {
        let dir = repo_root.join(format!("d{:03}", i % 20));
        let path = dir.join(format!("f{i:04}.txt"));
        std::fs::write(&path, format!("changed {i}\n")).expect("modify");
    }
    std::fs::remove_file(repo_root.join("swap.txt")).expect("remove file");
    std::fs::create_dir_all(repo_root.join("swap.txt")).expect("file to dir");
    std::fs::write(repo_root.join("swap.txt/leaf.txt"), b"leaf\n").expect("dir leaf");
    git(repo_root, &["add", "-A"]);
    git(repo_root, &["commit", "-qm", "target"]);
    git(repo_root, &["checkout", "-q", "main"]);
}

#[test]
fn switch_tree_diff_git_clean_after() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);

    populate_initial(tmp.path());
    build_target_branch(tmp.path());

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");

    let target_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "target"]).trim()).expect("target oid");

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "target".into(),
                oid: target_oid,
            },
        },
    )
    .expect("switch to target");

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after grit switch, got:\n{}",
        git_porcelain(tmp.path())
    );
    assert!(
        git_diff_index_head(tmp.path()).trim().is_empty(),
        "git diff-index HEAD must be empty, got:\n{}",
        git_diff_index_head(tmp.path())
    );
    git_fsck(tmp.path());

    let _ = refs::resolve_ref(grit_repo.git_dir(), "HEAD").expect("head");
}

#[test]
fn switch_refuses_dirty_conflicting_path_and_leaves_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("hot.txt"), b"main\n").expect("write");
    git(tmp.path(), &["add", "hot.txt"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "other"]);
    std::fs::write(tmp.path().join("hot.txt"), b"other\n").expect("write");
    git(tmp.path(), &["commit", "-am", "other"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    std::fs::write(tmp.path().join("hot.txt"), b"dirty local\n").expect("dirty");
    let before = std::fs::read(tmp.path().join("hot.txt")).expect("read");

    let grit_repo = Repository::open(
        &tmp.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(tmp.path()),
    )
    .expect("open grit repo");
    let other_oid =
        ObjectId::from_hex(git(tmp.path(), &["rev-parse", "other"]).trim()).expect("other oid");

    let err = switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "other".into(),
                oid: other_oid,
            },
        },
    )
    .expect_err("dirty switch must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("uncommitted changes"),
        "expected uncommitted changes error, got: {msg}"
    );
    let after = std::fs::read(tmp.path().join("hot.txt")).expect("read after");
    assert_eq!(before, after, "conflicting dirty file must be untouched");
}
