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
fn switch_nested_gitattributes_crlf_clean_worktree_matches_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);
    git(tmp.path(), &["config", "gc.auto", "0"]);
    git(tmp.path(), &["config", "core.autocrlf", "false"]);

    std::fs::create_dir_all(tmp.path().join("sub")).expect("mkdir sub");
    std::fs::write(
        tmp.path().join("sub/.gitattributes"),
        "*.txt text eol=crlf\n",
    )
    .expect("nested gitattributes");
    std::fs::write(tmp.path().join("sub/file.txt"), "main line\r\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "expected clean git status on main, got:\n{}",
        git_porcelain(tmp.path())
    );

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    std::fs::write(tmp.path().join("sub/file.txt"), "target line\r\n").expect("write target");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "expected clean git status before switch, got:\n{}",
        git_porcelain(tmp.path())
    );

    git(tmp.path(), &["switch", "-q", "target"]);
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "system git switch to target must leave clean tree"
    );
    git(tmp.path(), &["switch", "-q", "main"]);

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
    .expect("grit switch must succeed on clean nested-eol=crlf worktree like git");

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

#[cfg(unix)]
#[test]
fn switch_tree_diff_type_and_mode_round_trip_git_clean() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main", "."]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("run.sh"), b"#!/bin/sh\n").expect("write");
    std::fs::write(tmp.path().join("plain.txt"), b"file\n").expect("write");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "main"]);

    git(tmp.path(), &["checkout", "-q", "-b", "target"]);
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(tmp.path().join("run.sh"))
            .expect("meta")
            .permissions();
        perms.set_mode(0o100755);
        std::fs::set_permissions(tmp.path().join("run.sh"), perms).expect("chmod");
    }
    std::fs::remove_file(tmp.path().join("plain.txt")).expect("remove file");
    std::fs::create_dir_all(tmp.path().join("plain.txt")).expect("file to dir");
    std::fs::write(tmp.path().join("plain.txt/inner.txt"), b"inner\n").expect("inner");
    symlink("plain.txt/inner.txt", tmp.path().join("link.txt")).expect("symlink");
    git(tmp.path(), &["add", "-A"]);
    git(tmp.path(), &["commit", "-qm", "target"]);
    git(tmp.path(), &["checkout", "-q", "main"]);

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
    .expect("switch");

    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status --porcelain must be empty after type/mode switch, got:\n{}",
        git_porcelain(tmp.path())
    );
    assert!(
        git_diff_index_head(tmp.path()).trim().is_empty(),
        "git diff-index HEAD must be empty, got:\n{}",
        git_diff_index_head(tmp.path())
    );
    git_fsck(tmp.path());

    switch_repository(
        &grit_repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: "main".into(),
                oid: ObjectId::from_hex(git(tmp.path(), &["rev-parse", "main"]).trim())
                    .expect("main oid"),
            },
        },
    )
    .expect("switch back");
    assert!(
        git_porcelain(tmp.path()).trim().is_empty(),
        "git status must be clean after switch back"
    );
}
