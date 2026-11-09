//! Regression for issue #2570: stash apply/pop must remove tracked files deleted in the stash.

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::progress::{CancelToken, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::stash::{StashApplyOptions, StashPushOptions};
use grit_test_support::git;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn push_opts() -> StashPushOptions {
    StashPushOptions {
        message: None,
        include_untracked: false,
        identity: "Test <t@example.com> 1000000000 +0000"
            .parse()
            .expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: grit_lib::stash::StashMode::default(),
    }
}

#[test]
fn apply_unstaged_tracked_deletion_matches_git() {
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_repo(root);
        std::fs::write(root.join("x.txt"), "x\n").expect("write");
        git_cmd(root, &["add", "x.txt"]);
        git_cmd(root, &["commit", "-qm", "base"]);
        std::fs::remove_file(root.join("x.txt")).expect("unstaged delete");
    }

    open_grit(grit_root.path())
        .stash()
        .push(&push_opts(), &mut NullProgress, &CancelToken::never())
        .expect("push")
        .expect("changes");
    git_cmd(git_root.path(), &["stash", "push", "-q"]);

    assert!(grit_root.path().join("x.txt").is_file());
    open_grit(grit_root.path())
        .stash()
        .apply(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("apply");
    git_cmd(git_root.path(), &["stash", "apply", "-q"]);

    assert!(!grit_root.path().join("x.txt").exists());
    assert_eq!(
        git_cmd(grit_root.path(), &["status", "--porcelain=v1"]),
        git_cmd(git_root.path(), &["status", "--porcelain=v1"])
    );
    let porcelain = git_cmd(grit_root.path(), &["status", "--porcelain=v1"]);
    assert!(
        porcelain.contains(" D x.txt"),
        "expected unstaged deletion, got:\n{porcelain}"
    );
}

#[test]
fn pop_unstaged_tracked_deletion_matches_git() {
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_repo(root);
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("sub/n.txt"), "n\n").expect("write");
        git_cmd(root, &["add", "sub/n.txt"]);
        git_cmd(root, &["commit", "-qm", "base"]);
        std::fs::remove_file(root.join("sub/n.txt")).expect("unstaged delete");
    }

    open_grit(grit_root.path())
        .stash()
        .push(&push_opts(), &mut NullProgress, &CancelToken::never())
        .expect("push")
        .expect("changes");
    git_cmd(git_root.path(), &["stash", "push", "-q"]);

    open_grit(grit_root.path())
        .stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("pop");
    git_cmd(git_root.path(), &["stash", "pop", "-q"]);

    assert!(!grit_root.path().join("sub/n.txt").exists());
    assert!(git_cmd(grit_root.path(), &["stash", "list"])
        .trim()
        .is_empty());
}

#[test]
fn apply_git_created_stash_with_tracked_deletion() {
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_repo(root);
        std::fs::write(root.join("z.txt"), "z\n").expect("write");
        git_cmd(root, &["add", "z.txt"]);
        git_cmd(root, &["commit", "-qm", "base"]);
        std::fs::remove_file(root.join("z.txt")).expect("unstaged delete");
        git_cmd(root, &["stash", "push", "-q"]);
    }

    open_grit(grit_root.path())
        .stash()
        .apply(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &CancelToken::never(),
        )
        .expect("apply git stash");
    assert!(!grit_root.path().join("z.txt").exists());
    git_cmd(git_root.path(), &["stash", "apply", "-q"]);
    assert_eq!(
        git_cmd(grit_root.path(), &["status", "--porcelain=v1"]),
        git_cmd(git_root.path(), &["status", "--porcelain=v1"])
    );
}
