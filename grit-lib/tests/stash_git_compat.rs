//! Round-trip stash create/list/pop/drop with system `git`.

use grit_lib::diff::DiffStatus;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::branch_error::BranchError;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::stash::{StashApplyOptions, StashDiffOptions, StashPushOptions};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::reflog::read_reflog;
use grit_lib::repo::Repository;
use grit_lib::worktree::{add_linked_worktree, AddLinkedWorktreeOptions, WorktreeBranch};
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

fn stash_options(include_untracked: bool, message: Option<&str>) -> StashPushOptions {
    StashPushOptions {
        message: message.map(str::to_owned),
        include_untracked,
        identity: "Test <t@example.com> 1000000000 +0000"
            .parse()
            .expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: grit_lib::porcelain::stash::StashMode::default(),
    }
}

fn assert_fsck_strict(root: &std::path::Path) {
    let fsck = git_cmd(root, &["fsck", "--strict"]);
    assert!(
        !fsck.to_lowercase().contains("error"),
        "fsck --strict must be clean:\n{fsck}"
    );
}

#[test]
fn grit_stash_create_matches_git_list_show_pop_and_fsck() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("tracked.txt"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "tracked.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("tracked.txt"), "unstaged\n").expect("modify");
    std::fs::write(dir.path().join("staged.txt"), "staged\n").expect("staged new");
    git_cmd(dir.path(), &["add", "staged.txt"]);

    let repo = open_grit(dir.path());
    let oid = repo
        .stash()
        .push(&stash_options(false, Some("custom msg")))
        .expect("push")
        .expect("had changes")
        .oid;

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push_stash must leave a clean tree before git pop, got:\n{clean}"
    );

    let list = git_cmd(dir.path(), &["stash", "list"]);
    assert!(
        list.contains("custom msg") || list.contains("On main: custom msg"),
        "git stash list should show grit message:\n{list}"
    );

    let grit_show = git_cmd(
        dir.path(),
        &["stash", "show", "-p", &format!("{}", oid.to_hex())],
    );
    let git_show = git_cmd(dir.path(), &["stash", "show", "-p", "stash@{0}"]);
    assert_eq!(
        grit_show.trim(),
        git_show.trim(),
        "stash show -p must match"
    );

    git_cmd(dir.path(), &["stash", "pop", "--index"]);

    let porcelain = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.contains("tracked.txt"),
        "worktree change restored"
    );
    assert!(
        porcelain.contains("staged.txt"),
        "index restored with --index"
    );

    let fsck = git_cmd(dir.path(), &["fsck", "--strict"]);
    assert!(
        !fsck.to_lowercase().contains("error"),
        "fsck --strict must be clean:\n{fsck}"
    );
}

#[test]
fn grit_stash_with_untracked_matches_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "a\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("u.txt"), "untracked\n").expect("untracked");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("stash");

    assert!(
        !dir.path().join("u.txt").exists(),
        "push should remove stashed untracked file"
    );
    git_cmd(dir.path(), &["stash", "pop"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("restored"),
        "untracked\n"
    );
}

#[test]
fn git_stash_list_order_visible_to_grit_pop_and_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "v1\n").expect("write");
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("f"), "one\n").expect("one");
    git_cmd(dir.path(), &["stash", "push", "-m", "first"]);
    std::fs::write(dir.path().join("f"), "two\n").expect("two");
    git_cmd(dir.path(), &["stash", "push", "-u", "-m", "second"]);

    let repo = open_grit(dir.path());
    let listed = repo.stash().list().expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].index, 0);
    assert!(listed[0].message.contains("second") || listed[1].message.contains("second"));

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop newest");

    let git_list = git_cmd(dir.path(), &["stash", "list"]);
    assert!(
        git_list.contains("first") && !git_list.contains("second"),
        "after pop, only older stash remains:\n{git_list}"
    );

    repo.stash().drop(0).expect("drop last");
    let list_after = git_cmd(dir.path(), &["stash", "list"]);
    assert!(list_after.trim().is_empty(), "refs/stash should be gone");
}

#[test]
fn push_stash_clears_index_when_worktree_matches_head_but_index_staged() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("f"), "staged\n").expect("stage");
    git_cmd(dir.path(), &["add", "f"]);
    std::fs::write(dir.path().join("f"), "base\n").expect("revert worktree only");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("had staged index change");

    let porcelain = git_cmd(dir.path(), &["status", "--porcelain=v1"]);
    assert!(
        porcelain.trim().is_empty(),
        "push_stash must leave clean index and worktree, got:\n{porcelain}"
    );
}

#[test]
fn drop_stash_oldest_entry_rechains_reflog_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("t"), "v\n").expect("write");
    git_cmd(dir.path(), &["add", "t"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    for msg in ["one", "two", "three"] {
        std::fs::write(dir.path().join("t"), format!("{msg}\n")).expect("write");
        git_cmd(dir.path(), &["stash", "push", "-m", msg]);
    }

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(dir.path(), grit_dir.path());
    copy_repo(dir.path(), git_dir.path());

    (open_grit(grit_dir.path()))
        .stash()
        .drop(2)
        .expect("grit drop oldest");
    git_cmd(git_dir.path(), &["stash", "drop", "stash@{2}"]);

    let grit_log =
        std::fs::read_to_string(grit_dir.path().join(".git/logs/refs/stash")).expect("grit reflog");
    let git_log =
        std::fs::read_to_string(git_dir.path().join(".git/logs/refs/stash")).expect("git reflog");
    assert_eq!(
        grit_log, git_log,
        "oldest stash drop must rewrite the reflog like git"
    );
}

#[test]
fn drop_stash_middle_entry_rechains_reflog_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("t"), "v\n").expect("write");
    git_cmd(dir.path(), &["add", "t"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    for msg in ["one", "two", "three"] {
        std::fs::write(dir.path().join("t"), format!("{msg}\n")).expect("write");
        git_cmd(dir.path(), &["stash", "push", "-m", msg]);
    }

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(dir.path(), grit_dir.path());
    copy_repo(dir.path(), git_dir.path());

    (open_grit(grit_dir.path()))
        .stash()
        .drop(1)
        .expect("grit drop middle");
    git_cmd(git_dir.path(), &["stash", "drop", "stash@{1}"]);

    let grit_log =
        std::fs::read_to_string(grit_dir.path().join(".git/logs/refs/stash")).expect("grit reflog");
    let git_log =
        std::fs::read_to_string(git_dir.path().join(".git/logs/refs/stash")).expect("git reflog");
    assert_eq!(
        grit_log, git_log,
        "middle stash drop must rewrite the reflog like git"
    );

    let entries = read_reflog(&grit_dir.path().join(".git"), "refs/stash").expect("parse");
    for window in entries.windows(2) {
        assert_eq!(
            window[0].new_oid, window[1].old_oid,
            "reflog chain must connect after drop"
        );
    }
}

fn copy_repo(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    let status = std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(status.success());
}

#[test]
fn create_stash_returns_none_when_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("x"), "x\n").expect("write");
    git_cmd(dir.path(), &["add", "x"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    let repo = open_grit(dir.path());
    let none = repo
        .stash()
        .create(&stash_options(false, None))
        .expect("create");
    assert!(none.is_none());
}

#[test]
fn pop_keeps_entry_on_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("c"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "c"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("c"), "stashed side\n").expect("stash me");
    git_cmd(dir.path(), &["stash", "push", "-m", "conflict-test"]);

    std::fs::write(dir.path().join("c"), "moved head side\n").expect("head change");
    git_cmd(dir.path(), &["add", "c"]);
    git_cmd(dir.path(), &["commit", "-qm", "move head"]);

    let repo = open_grit(dir.path());
    let conflicts = repo
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop with conflict");
    assert!(
        conflicts.has_conflicts(),
        "three-way conflict should keep stash entry"
    );
    assert_eq!(repo.stash().list().expect("list").len(), 1);
}

#[test]
fn pop_stash_with_index_restores_staged_deletion_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "content\n").expect("write");
    git_cmd(dir.path(), &["add", "f"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    git_cmd(dir.path(), &["rm", "f"]);
    git_cmd(dir.path(), &["stash", "push", "-m", "staged rm"]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(dir.path(), grit_dir.path());
    copy_repo(dir.path(), git_dir.path());

    (open_grit(grit_dir.path()))
        .stash()
        .pop(
            0,
            &StashApplyOptions {
                restore_index: true,
            },
        )
        .expect("grit pop --index");
    git_cmd(git_dir.path(), &["stash", "pop", "--index"]);

    let grit_p = git_cmd(grit_dir.path(), &["status", "--porcelain=v1"]);
    let git_p = git_cmd(git_dir.path(), &["status", "--porcelain=v1"]);
    assert_eq!(
        grit_p, git_p,
        "staged deletion must match git stash pop --index"
    );
    assert!(
        grit_p.starts_with("D  f"),
        "expected staged deletion D  f, got:\n{grit_p}"
    );
}

#[test]
fn pop_stash_restores_untracked_symlink_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("tracked"), "t\n").expect("write");
    git_cmd(dir.path(), &["add", "tracked"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::os::unix::fs::symlink("target", dir.path().join("link")).expect("symlink");
    git_cmd(dir.path(), &["stash", "push", "-u", "-m", "symlink"]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(dir.path(), grit_dir.path());
    copy_repo(dir.path(), git_dir.path());

    (open_grit(grit_dir.path()))
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect("grit pop");
    git_cmd(git_dir.path(), &["stash", "pop"]);

    let grit_link = grit_dir.path().join("link");
    let git_link = git_dir.path().join("link");
    assert!(grit_link.is_symlink(), "grit must restore symlink");
    assert!(git_link.is_symlink(), "git must restore symlink");
    assert_eq!(
        std::fs::read_link(&grit_link).expect("read grit"),
        std::fs::read_link(&git_link).expect("read git")
    );

    let grit_list = git_cmd(grit_dir.path(), &["stash", "list"]);
    let git_list = git_cmd(git_dir.path(), &["stash", "list"]);
    assert_eq!(
        grit_list.trim(),
        git_list.trim(),
        "stash entry dropped after pop"
    );
}

#[test]
fn stash_pop_with_untracked_does_not_overwrite_existing_untracked() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "a\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("u.txt"), "stashed-untracked\n").expect("untracked");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, Some("m")))
        .expect("push")
        .expect("stash");
    std::fs::write(dir.path().join("u.txt"), "PRECIOUS new work\n").expect("replace");

    let err = repo
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect_err("pop must fail");
    assert!(
        matches!(
            err,
            Error::StashCouldNotRestoreUntracked { ref path } if path == "u.txt"
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "PRECIOUS new work\n"
    );
    assert_eq!(repo.stash().list().expect("list").len(), 1);

    let err = repo
        .stash()
        .apply_commit(
            &repo.stash().list().expect("list")[0].oid,
            &StashApplyOptions::default(),
        )
        .expect_err("apply must fail");
    assert!(
        matches!(err, Error::StashCouldNotRestoreUntracked { .. }),
        "apply error: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "PRECIOUS new work\n"
    );

    let git_pop_fail = Command::new("git")
        .args(["stash", "pop"])
        .current_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git stash pop");
    assert!(
        !git_pop_fail.status.success(),
        "git must refuse when untracked path exists"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "PRECIOUS new work\n"
    );

    std::fs::remove_file(dir.path().join("u.txt")).expect("remove");
    git_cmd(dir.path(), &["stash", "pop"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read after git"),
        "stashed-untracked\n"
    );
}

#[test]
fn stash_apply_with_untracked_does_not_overwrite_ignored_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "a\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("u.txt"), "stashed-untracked\n").expect("untracked");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("stash");
    std::fs::write(dir.path().join(".gitignore"), "u.txt\n").expect("ignore");
    std::fs::write(dir.path().join("u.txt"), "precious\n").expect("ignored precious");

    let err = repo
        .stash()
        .apply_commit(
            &repo.stash().list().expect("list")[0].oid,
            &StashApplyOptions::default(),
        )
        .expect_err("apply must fail on ignored collision");
    assert!(
        matches!(
            err,
            Error::StashCouldNotRestoreUntracked { ref path } if path == "u.txt"
        ),
        "unexpected: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "precious\n"
    );

    let git_apply = Command::new("git")
        .args(["stash", "apply"])
        .current_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git stash apply");
    assert!(
        !git_apply.status.success(),
        "git must refuse when ignored path exists"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "precious\n"
    );
}

#[test]
fn stash_apply_with_untracked_does_not_overwrite_newly_tracked_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "a\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("u.txt"), "stashed-untracked\n").expect("untracked");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("stash");
    std::fs::write(dir.path().join("u.txt"), "precious\n").expect("precious");
    git_cmd(dir.path(), &["add", "u.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "track u"]);

    let err = repo
        .stash()
        .apply_commit(
            &repo.stash().list().expect("list")[0].oid,
            &StashApplyOptions::default(),
        )
        .expect_err("apply must fail on tracked collision");
    assert!(
        matches!(
            err,
            Error::StashCouldNotRestoreUntracked { ref path } if path == "u.txt"
        ),
        "unexpected: {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "precious\n"
    );

    let git_apply = Command::new("git")
        .args(["stash", "apply"])
        .current_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git stash apply");
    assert!(
        !git_apply.status.success(),
        "git must refuse when tracked path exists"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("u.txt")).expect("read"),
        "precious\n"
    );
}

#[test]
fn stash_diff_matches_git_show_stat_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("d"), "d\n").expect("write");
    git_cmd(dir.path(), &["add", "d"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("d"), "changed\n").expect("change");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("oid");

    let diffs = repo
        .stash()
        .diff(0, &StashDiffOptions::default())
        .expect("diff");
    assert!(
        diffs.iter().any(|d| d.path().contains("d")),
        "stash diff should include changed path"
    );
}

#[test]
fn t3903_grit_stash_read_by_git_and_reverse() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "v0\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), "grit-stash\n").expect("modify");
    std::fs::write(dir.path().join("new.txt"), "added\n").expect("new");
    git_cmd(dir.path(), &["add", "new.txt"]);

    let repo = open_grit(dir.path());
    let oid = repo
        .stash()
        .push(&stash_options(false, Some("grit msg")))
        .expect("push")
        .expect("changes")
        .oid;
    assert_fsck_strict(dir.path());

    let grit_show = git_cmd(
        dir.path(),
        &["stash", "show", "-p", &format!("{}", oid.to_hex())],
    );
    let git_show = git_cmd(dir.path(), &["stash", "show", "-p", "stash@{0}"]);
    assert_eq!(grit_show.trim(), git_show.trim());

    let grit_dir = tempfile::tempdir().expect("grit copy");
    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(dir.path(), grit_dir.path());
    copy_repo(dir.path(), git_dir.path());

    std::fs::write(git_dir.path().join("a.txt"), "v0\n").expect("reset wt");
    git_cmd(git_dir.path(), &["reset", "--hard", "HEAD"]);

    let stash_hex = git_cmd(git_dir.path(), &["rev-parse", "refs/stash"])
        .trim()
        .to_owned();
    let stash_oid = ObjectId::from_hex(&stash_hex).expect("stash oid");
    (open_grit(grit_dir.path()))
        .stash()
        .apply_commit(
            &stash_oid,
            &StashApplyOptions {
                restore_index: true,
            },
        )
        .expect("grit apply git stash");
    git_cmd(git_dir.path(), &["stash", "apply", "--index", "stash@{0}"]);

    let grit_p = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_p = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(grit_p, git_p, "git-created stash apply must match git");
    assert_fsck_strict(grit_dir.path());
}

/// Untracked embedded repositories must not be stashed or deleted (`git stash push -u` ignores them).
#[test]
fn stash_push_u_preserves_untracked_nested_repository() {
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_repo(root);
        std::fs::write(root.join("README"), "root\n").expect("write");
        git_cmd(root, &["add", "README"]);
        git_cmd(root, &["commit", "-qm", "init"]);
        let inner = root.join("inner");
        std::fs::create_dir_all(&inner).expect("mkdir inner");
        git_cmd(&inner, &["init", "-q"]);
        git_cmd(&inner, &["config", "user.email", "nested@example.com"]);
        git_cmd(&inner, &["config", "user.name", "Nested"]);
        std::fs::write(inner.join("nested.txt"), "inside nested repo\n").expect("nested file");
        git_cmd(&inner, &["add", "nested.txt"]);
        git_cmd(&inner, &["commit", "-qm", "nested init"]);
        std::fs::write(inner.join("other.txt"), "outer-untracked\n").expect("outer untracked");
    }

    (open_grit(grit_root.path()))
        .stash()
        .push(&stash_options(true, None))
        .expect("grit push")
        .expect("had changes");
    git_cmd(git_root.path(), &["stash", "push", "-u"]);

    for root in [grit_root.path(), git_root.path()] {
        let inner = root.join("inner");
        assert!(
            inner.join(".git").exists(),
            "nested .git must survive stash -u: {}",
            root.display()
        );
        assert!(
            inner.join("nested.txt").exists(),
            "committed nested-repo file must not be deleted: {}",
            root.display()
        );
    }

    let grit_status = git_cmd(grit_root.path(), &["status", "--porcelain"]);
    let git_status = git_cmd(git_root.path(), &["status", "--porcelain"]);
    assert_eq!(
        grit_status.trim(),
        git_status.trim(),
        "porcelain after stash -u with nested repo must match git"
    );
    assert!(
        grit_status.contains("?? inner/"),
        "nested repo should remain untracked: {grit_status}"
    );
    assert_fsck_strict(grit_root.path());
}

#[test]
fn t3905_include_untracked_stash_shape_matches_git() {
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_repo(root);
        std::fs::write(root.join("tracked"), "t\n").expect("write");
        git_cmd(root, &["add", "tracked"]);
        git_cmd(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("tracked"), "changed\n").expect("modify");
        std::fs::write(root.join("untracked.txt"), "u\n").expect("untracked");
    }

    (open_grit(grit_root.path()))
        .stash()
        .push(&stash_options(true, Some("with-u")))
        .expect("grit push")
        .expect("had changes");
    git_cmd(git_root.path(), &["stash", "push", "-u", "-m", "with-u"]);

    let grit_show = git_cmd(grit_root.path(), &["stash", "show", "-p", "stash@{0}"]);
    let git_show = git_cmd(git_root.path(), &["stash", "show", "-p", "stash@{0}"]);
    assert_eq!(
        grit_show.trim(),
        git_show.trim(),
        "include-untracked stash shape must match git"
    );
    assert_fsck_strict(grit_root.path());
}

#[test]
fn t3908_stash_in_linked_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    std::fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    std::fs::write(main.join("f"), "base\n").expect("write");
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let linked = tmp.path().join("linked");
    add_linked_worktree(
        &open_grit(&main),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some(WorktreeBranch::Create("linked".into())),
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("add worktree");

    std::fs::write(linked.join("f"), "linked change\n").expect("modify");
    (Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty())
        .expect("discover linked"))
    .stash()
    .push(&stash_options(false, Some("from-linked")))
    .expect("stash from linked")
    .expect("changes");

    let list = git_cmd(&main, &["stash", "list"]);
    assert!(
        list.contains("from-linked") || list.contains("linked"),
        "main repo must see linked worktree stash:\n{list}"
    );
    assert_fsck_strict(&main);
}

/// Tracked directory replaced by an untracked file (`git stash push -u` must reset to HEAD).
#[test]
fn stash_push_u_dir_replaced_by_untracked_file_leaves_clean_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::create_dir_all(dir.path().join("d/e")).expect("mkdir");
    std::fs::write(dir.path().join("d/c.txt"), "3\n").expect("write");
    std::fs::write(dir.path().join("d/e/f.txt"), "4\n").expect("write");
    git_cmd(dir.path(), &["add", "-A"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::remove_dir_all(dir.path().join("d")).expect("rm tree");
    std::fs::write(dir.path().join("d"), "f\n").expect("untracked file");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("stash oid");

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push -u must leave a clean tree like git stash, got:\n{clean}"
    );
    assert!(dir.path().join("d/c.txt").is_file());
    assert!(dir.path().join("d/e/f.txt").is_file());

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");

    let restored = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(restored.contains("d/c.txt"), "deleted paths restored");
    assert!(
        restored.contains("?? d") || restored.contains(" d"),
        "untracked file back"
    );
    assert_fsck_strict(dir.path());
}

/// Tracked file replaced by an untracked directory: pop must refuse and keep the stash.
#[test]
fn stash_push_u_tracked_file_replaced_by_untracked_dir_pop_refuses() {
    use grit_lib::objects::parse_commit;

    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "1\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::remove_file(dir.path().join("a.txt")).expect("rm file");
    std::fs::create_dir_all(dir.path().join("a.txt")).expect("mkdir");
    std::fs::write(dir.path().join("a.txt/x"), "in\n").expect("nested");

    let repo = open_grit(dir.path());
    let stash_oid = repo
        .stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("stash oid")
        .oid;

    let stash_obj = repo.odb().read(&stash_oid).expect("read stash");
    let stash_commit = parse_commit(&stash_obj.data).expect("parse stash");
    assert_eq!(
        stash_commit.parents.len(),
        3,
        "stash with -u and nested untracked under type swap needs three parents"
    );

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push -u must reset worktree, got:\n{clean}"
    );
    assert!(dir.path().join("a.txt").is_file());

    let err = repo
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect_err("pop must fail like git when untracked dir cannot be restored");
    assert!(
        matches!(err, Error::StashCouldNotRestoreUntracked { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(repo.stash().list().expect("list").len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("file kept"),
        "1\n"
    );
    assert_fsck_strict(dir.path());
}

#[test]
#[cfg(unix)]
fn push_stash_clears_unstaged_exec_bit_only_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("b.txt"), "1\n").expect("write");
    git_cmd(dir.path(), &["add", "b.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir.path().join("b.txt"))
            .expect("stat")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path().join("b.txt"), perms).expect("chmod");
    }

    let porcelain_before = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain_before.contains("b.txt"),
        "chmod +x must dirty worktree before stash:\n{porcelain_before}"
    );

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("had mode-only change");

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push_stash must reset exec-bit-only dirt before pop, got:\n{clean}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("b.txt"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "worktree must match HEAD non-executable mode");
    }

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("b.txt"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755, "pop must restore executable bit");
    }

    assert_fsck_strict(dir.path());
}

#[test]
#[cfg(unix)]
fn push_stash_clears_unstaged_exec_bit_in_subdirectory() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::create_dir_all(dir.path().join("d")).expect("mkdir");
    std::fs::write(dir.path().join("d/c.txt"), "x\n").expect("write");
    git_cmd(dir.path(), &["add", "d/c.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join("d/c.txt");
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("stash");

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push_stash must leave clean tree for nested path, got:\n{clean}"
    );

    repo.stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");
}

#[test]
fn push_stash_normalizes_crlf_worktree_under_gitattributes_eol_crlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
    std::fs::write(dir.path().join("f.txt"), b"a\r\nb\r\n").expect("write");
    git_cmd(dir.path(), &["add", "."]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);

    std::fs::write(dir.path().join("f.txt"), b"a\r\nb\r\nc\r\n").expect("modify");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("had changes");

    let stash_blob = git_cmd(dir.path(), &["cat-file", "-p", "stash@{0}:f.txt"]);
    let head_blob = git_cmd(dir.path(), &["cat-file", "-p", "HEAD:f.txt"]);
    assert_eq!(
        stash_blob, "a\nb\nc\n",
        "stash worktree tree must store LF-normalized blob like git stash"
    );
    assert_eq!(head_blob, "a\nb\n", "HEAD stays LF from commit");

    let diff_stat = git_cmd(dir.path(), &["diff", "--stat", "HEAD", "stash@{0}"]);
    assert!(
        diff_stat.contains("1 insertion"),
        "git diff --stat should show one line added, got:\n{diff_stat}"
    );
    assert!(
        !diff_stat.contains("2 deletion"),
        "whole-file rewrite must not appear:\n{diff_stat}"
    );

    assert_fsck_strict(dir.path());
}

#[test]
fn push_stash_untracked_normalizes_crlf_under_gitattributes_eol_crlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join(".gitattributes"), "*.txt text eol=crlf\n").expect("attrs");
    std::fs::write(dir.path().join("tracked.txt"), b"x\r\n").expect("write");
    git_cmd(dir.path(), &["add", "."]);
    git_cmd(dir.path(), &["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("new.txt"), b"n\r\n").expect("untracked");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("had changes");

    let stash_blob = git_cmd(dir.path(), &["cat-file", "-p", "stash^3:new.txt"]);
    assert_eq!(
        stash_blob, "n\n",
        "untracked stash parent must LF-normalize under eol=crlf"
    );
    assert_fsck_strict(dir.path());
}

fn git_stash_name_status(
    repo: &std::path::Path,
    index: &str,
    include_untracked: bool,
) -> Vec<(char, String)> {
    let mut args = vec!["stash", "show", "--name-status"];
    if include_untracked {
        args.push("-u");
    }
    args.push(index);
    git_cmd(repo, &args)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let status = parts.next()?.chars().next()?;
            let path = parts.next()?.to_owned();
            Some((status, path))
        })
        .collect()
}

fn grit_stash_name_status(
    repo: &Repository,
    n: usize,
    include_untracked: bool,
) -> Vec<(char, String)> {
    let diffs = repo
        .stash()
        .diff(n, &StashDiffOptions { include_untracked })
        .expect("stash diff");
    diffs
        .into_iter()
        .map(|entry| {
            let status = match entry.status {
                DiffStatus::Added => 'A',
                DiffStatus::Deleted => 'D',
                DiffStatus::Modified | DiffStatus::TypeChanged => 'M',
                DiffStatus::Renamed => 'R',
                DiffStatus::Copied => 'C',
                _ => '?',
            };
            (status, entry.path().to_owned())
        })
        .collect()
}

#[test]
fn stash_show_untracked_lists_u_commit_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("tracked.txt"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "tracked.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("tracked.txt"), "dirty\n").expect("modify");
    std::fs::write(dir.path().join("untracked.txt"), "new\n").expect("untracked");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("changes");

    let git_lines = git_stash_name_status(dir.path(), "stash@{0}", true);
    let grit_lines = grit_stash_name_status(&repo, 0, true);
    assert_eq!(
        git_lines, grit_lines,
        "stash show -u must match git name-status"
    );
}

#[test]
fn stash_branch_creates_branch_at_base_and_restores_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "f.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("f.txt"), "worktree\n").expect("wt");
    std::fs::write(dir.path().join("staged.txt"), "staged\n").expect("staged");
    git_cmd(dir.path(), &["add", "staged.txt"]);

    let repo = open_grit(dir.path());
    let base = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    repo.stash()
        .push(&stash_options(false, Some("branch me")))
        .expect("push")
        .expect("changes");

    let outcome = repo.stash().branch("from-stash", 0).expect("stash branch");
    assert!(!outcome.conflicts);
    assert_eq!(outcome.base.to_hex(), base);
    assert_eq!(
        git_cmd(dir.path(), &["rev-parse", "from-stash"]).trim(),
        base
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
        "worktree\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("staged.txt")).expect("read staged"),
        "staged\n"
    );
    let mut progress = NullProgress;
    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut progress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        model.staged.iter().any(|e| e.path() == "staged.txt"),
        "index must restore staged.txt"
    );
    assert!(repo.stash().list().expect("list").is_empty());
    assert!(git_cmd(dir.path(), &["stash", "list"]).trim().is_empty());
    assert_fsck_strict(dir.path());
}

#[test]
fn stash_branch_existing_name_is_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "v\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    git_cmd(dir.path(), &["branch", "taken"]);
    std::fs::write(dir.path().join("a"), "dirty\n").expect("dirty");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("changes");
    let err = repo.stash().branch("taken", 0).unwrap_err();
    assert!(matches!(
        err,
        Error::Branch(BranchError::AlreadyExists { name }) if name == "taken"
    ));
}

#[test]
fn stash_clear_removes_ref_and_reflog_git_agrees() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "v\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    let repo = open_grit(dir.path());
    std::fs::write(dir.path().join("a"), "one\n").expect("one");
    repo.stash()
        .push(&stash_options(false, Some("first")))
        .expect("push")
        .expect("changes");
    std::fs::write(dir.path().join("a"), "two\n").expect("two");
    repo.stash()
        .push(&stash_options(false, Some("second")))
        .expect("push")
        .expect("changes");

    let cleared = repo.stash().clear().expect("clear");
    assert_eq!(cleared, 2);
    assert!(git_cmd(dir.path(), &["stash", "list"]).trim().is_empty());
    let reflog = read_reflog(dir.path().join(".git").as_path(), "refs/stash").unwrap_or_default();
    assert!(reflog.is_empty(), "stash reflog must be empty after clear");
    assert_fsck_strict(dir.path());
}

#[test]
fn stash_clear_removes_ref_without_reflog_like_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a"), "v\n").expect("write");
    git_cmd(dir.path(), &["add", "a"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    let head = git_cmd(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    git_cmd(dir.path(), &["update-ref", "refs/stash", &head]);
    assert!(
        git_cmd(dir.path(), &["rev-parse", "--verify", "refs/stash"]).trim() == head,
        "refs/stash must exist before clear"
    );

    let repo = open_grit(dir.path());
    let cleared = repo.stash().clear().expect("clear");
    assert_eq!(
        cleared, 0,
        "reflog count is zero when ref was created without reflog"
    );
    let verify = Command::new("git")
        .args(["rev-parse", "--verify", "refs/stash"])
        .current_dir(dir.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git rev-parse");
    assert!(
        !verify.status.success(),
        "refs/stash must be deleted even without a reflog"
    );
    assert_fsck_strict(dir.path());
}

#[test]
fn stash_show_untracked_lists_u_path_in_base_tree_when_not_in_w_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("shared.txt"), "base\n").expect("write");
    git_cmd(dir.path(), &["add", "shared.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    git_cmd(dir.path(), &["rm", "--cached", "shared.txt"]);
    std::fs::write(dir.path().join("shared.txt"), "worktree\n").expect("wt");
    std::fs::write(dir.path().join("only-u.txt"), "u-only\n").expect("u-only");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push")
        .expect("changes");

    let git_lines = git_stash_name_status(dir.path(), "stash@{0}", true);
    let grit_lines = grit_stash_name_status(&repo, 0, true);
    assert_eq!(
        git_lines, grit_lines,
        "stash show -u must match git when U paths overlap base tree"
    );
}

/// Build the same committed tree of `dirs * per_dir` files plus `bin/tool.sh` in `root`.
fn init_wide_repo(root: &std::path::Path, dirs: usize, per_dir: usize) {
    init_repo(root);
    for d in 0..dirs {
        let dir = root.join(format!("d{d:02}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..per_dir {
            std::fs::write(dir.join(format!("f{f:02}.txt")), format!("{d}/{f}\n")).expect("write");
        }
    }
    std::fs::create_dir_all(root.join("bin")).expect("mkdir bin");
    std::fs::write(root.join("bin/tool.sh"), "#!/bin/sh\necho tool\n").expect("write tool");
    git_cmd(root, &["add", "-A"]);
    git_cmd(root, &["commit", "-qm", "init"]);
}

/// Unstaged edits in several directories, a mode-only change alone in its directory, and one
/// staged edit.
fn dirty_wide_repo(root: &std::path::Path, dirs: usize, per_dir: usize) {
    for d in (0..dirs).step_by(2) {
        for f in 0..per_dir {
            let path = root.join(format!("d{d:02}/f{f:02}.txt"));
            let mut body = std::fs::read_to_string(&path).expect("read");
            body.push_str("changed\n");
            std::fs::write(&path, body).expect("modify");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("bin/tool.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }
    std::fs::write(root.join("d01/f00.txt"), "staged\n").expect("stage edit");
    git_cmd(root, &["add", "d01/f00.txt"]);
}

fn rev_parse(root: &std::path::Path, rev: &str) -> String {
    git_cmd(root, &["rev-parse", rev]).trim().to_owned()
}

/// Push and pop on a clean-stat index with a valid cache-tree must record the same W and I
/// trees as `git stash` and leave the worktree, index and executable bits as git does.
#[test]
fn stash_push_pop_wide_tree_matches_git_trees_and_status() {
    let (dirs, per_dir) = (12, 25);
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_wide_repo(root, dirs, per_dir);
        dirty_wide_repo(root, dirs, per_dir);
    }

    let repo = open_grit(grit_root.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("changes");
    git_cmd(git_root.path(), &["stash", "push", "-q"]);
    for rev in ["refs/stash^{tree}", "refs/stash^2^{tree}"] {
        assert_eq!(
            rev_parse(grit_root.path(), rev),
            rev_parse(git_root.path(), rev),
            "{rev} must match git"
        );
    }
    assert_eq!(
        git_cmd(grit_root.path(), &["status", "--porcelain"]),
        "",
        "push must leave a clean worktree"
    );
    assert_fsck_strict(grit_root.path());

    let repo = open_grit(grit_root.path());
    let outcome = repo
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");
    assert!(!outcome.has_conflicts());
    git_cmd(git_root.path(), &["stash", "pop", "-q"]);

    for args in [
        &["status", "--porcelain"][..],
        &["diff"][..],
        &["diff", "--cached"][..],
        &["stash", "list"][..],
    ] {
        assert_eq!(
            git_cmd(grit_root.path(), args),
            git_cmd(git_root.path(), args),
            "git {} after pop must match git",
            args.join(" ")
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(grit_root.path().join("bin/tool.sh"))
            .expect("stat tool")
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "pop must restore the executable bit");
    }
    assert_fsck_strict(grit_root.path());
}

/// Pop must keep the directory of a deferred file write when a sibling in that directory is
/// deleted by the same stash (the directory is momentarily empty between the two passes).
#[test]
fn stash_pop_added_and_deleted_siblings_in_one_dir_matches_git() {
    let grit_root = tempfile::tempdir().expect("tempdir");
    let git_root = tempfile::tempdir().expect("tempdir");
    for root in [grit_root.path(), git_root.path()] {
        init_repo(root);
        std::fs::create_dir_all(root.join("dir")).expect("mkdir");
        std::fs::write(root.join("dir/z.txt"), "z\n").expect("write z");
        git_cmd(root, &["add", "dir/z.txt"]);
        git_cmd(root, &["commit", "-qm", "init"]);
        std::fs::write(root.join("dir/a.txt"), "a\n").expect("write a");
        git_cmd(root, &["add", "dir/a.txt"]);
        std::fs::remove_file(root.join("dir/z.txt")).expect("delete z");
    }

    let repo = open_grit(grit_root.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("changes");
    git_cmd(git_root.path(), &["stash", "push", "-q"]);

    let repo = open_grit(grit_root.path());
    let outcome = repo
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");
    assert!(!outcome.has_conflicts());
    git_cmd(git_root.path(), &["stash", "pop", "-q"]);

    assert_eq!(
        std::fs::read(grit_root.path().join("dir/a.txt")).expect("a.txt restored"),
        b"a\n"
    );
    assert!(!grit_root.path().join("dir/z.txt").exists());
    for args in [
        &["status", "--porcelain"][..],
        &["diff", "--cached"][..],
        &["stash", "list"][..],
    ] {
        assert_eq!(
            git_cmd(grit_root.path(), args),
            git_cmd(git_root.path(), args),
            "git {} after pop must match git",
            args.join(" ")
        );
    }
    assert_fsck_strict(grit_root.path());
}

/// A stashed path rewritten right after push (same size, typically within the index file's
/// mtime second) must still be detected as a local change: pop refuses and keeps the entry.
#[test]
fn stash_pop_refuses_racily_rewritten_same_size_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "aaaa\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    std::fs::write(dir.path().join("a.txt"), "bbbb\n").expect("modify");

    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(false, None))
        .expect("push")
        .expect("changes");
    assert_eq!(
        std::fs::read(dir.path().join("a.txt")).expect("read"),
        b"aaaa\n"
    );
    std::fs::write(dir.path().join("a.txt"), "cccc\n").expect("racy rewrite");

    let repo = open_grit(dir.path());
    let err = repo
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect_err("pop must refuse to overwrite the local change");
    assert!(
        matches!(err, Error::StashWouldOverwriteLocalChanges { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("a.txt")).expect("read"),
        b"cccc\n"
    );
    assert_eq!(repo.stash().list().expect("list").len(), 1);
}

/// Like `git stash`, stash commits are never signed, even with `commit.gpgsign=true`.
#[test]
fn stash_commits_are_unsigned_even_with_commit_gpgsign() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "v0\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    git_cmd(dir.path(), &["config", "commit.gpgsign", "true"]);
    git_cmd(dir.path(), &["config", "gpg.format", "openpgp"]);
    git_cmd(dir.path(), &["config", "gpg.program", "false"]);

    std::fs::write(dir.path().join("a.txt"), "git\n").expect("modify");
    git_cmd(dir.path(), &["stash", "push", "-q"]);

    std::fs::write(dir.path().join("a.txt"), "grit\n").expect("modify");
    std::fs::write(dir.path().join("u.txt"), "untracked\n").expect("untracked");
    let repo = open_grit(dir.path());
    repo.stash()
        .push(&stash_options(true, None))
        .expect("push must not try to sign")
        .expect("changes");

    for stash in ["stash@{0}", "stash@{1}"] {
        let parents = git_cmd(dir.path(), &["rev-list", "--parents", "-n", "1", stash]);
        for oid in parents.split_whitespace() {
            let raw = git_cmd(dir.path(), &["cat-file", "commit", oid]);
            assert!(
                !raw.contains("gpgsig"),
                "{stash} commit {oid} must be unsigned:\n{raw}"
            );
        }
    }
    assert_fsck_strict(dir.path());
}

/// Bulk checkout during stash apply must not persist the index until post-apply index
/// reconciliation runs (Bugbot: early index write skipped staged replay).
#[test]
fn stash_pop_many_files_without_index_matches_git() {
    const DIRS: usize = 40;
    const FILES_PER_DIR: usize = 25;

    let grit_dir = tempfile::tempdir().expect("tempdir");
    let git_dir = tempfile::tempdir().expect("tempdir");
    for root in [grit_dir.path(), git_dir.path()] {
        init_repo(root);
        for d in 0..DIRS {
            let dir = root.join(format!("d{d:02}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..FILES_PER_DIR {
                std::fs::write(dir.join(format!("f{f:02}.txt")), format!("base {d} {f}\n"))
                    .expect("write");
            }
        }
        git_cmd(root, &["add", "-A"]);
        git_cmd(root, &["commit", "-qm", "base"]);
        for d in 0..DIRS {
            for f in 0..FILES_PER_DIR {
                let path = root.join(format!("d{d:02}/f{f:02}.txt"));
                std::fs::write(&path, format!("stashed {d} {f}\n")).expect("edit");
            }
        }
    }

    git_cmd(git_dir.path(), &["stash", "push", "-q", "-m", "bulk"]);
    open_grit(grit_dir.path())
        .stash()
        .push(&stash_options(false, Some("bulk")))
        .expect("grit push")
        .expect("changes");

    git_cmd(git_dir.path(), &["stash", "pop", "-q"]);
    open_grit(grit_dir.path())
        .stash()
        .pop(
            0,
            &StashApplyOptions {
                restore_index: false,
            },
        )
        .expect("grit pop");

    assert_eq!(
        git_cmd(grit_dir.path(), &["status", "--porcelain"]),
        git_cmd(git_dir.path(), &["status", "--porcelain"]),
        "pop without --index must match git"
    );
    assert_eq!(
        git_cmd(grit_dir.path(), &["diff-files", "--name-status"]),
        git_cmd(git_dir.path(), &["diff-files", "--name-status"]),
        "index vs worktree must match git after bulk stash pop"
    );
    assert_fsck_strict(grit_dir.path());
}

#[test]
fn wide_sparse_pop_keeps_conflicts_and_unrelated_local_content() {
    let grit_dir = tempfile::tempdir().expect("grit");
    init_repo(grit_dir.path());
    for d in 0..30 {
        let dir = grit_dir.path().join(format!("d{d:03}"));
        std::fs::create_dir(&dir).expect("directory");
        for f in 0..100 {
            std::fs::write(dir.join(format!("f{f:03}")), "base\n").expect("file");
        }
    }
    git_cmd(grit_dir.path(), &["add", "."]);
    git_cmd(grit_dir.path(), &["commit", "-qm", "base"]);
    std::fs::write(grit_dir.path().join("d000/f000"), "stash\n").expect("stash edit");
    git_cmd(grit_dir.path(), &["stash", "push", "-qm", "conflict"]);
    std::fs::write(grit_dir.path().join("d000/f000"), "head\n").expect("head edit");
    git_cmd(grit_dir.path(), &["add", "d000/f000"]);
    #[cfg(unix)]
    git_cmd(
        grit_dir.path(),
        &["update-index", "--chmod=+x", "d000/f000"],
    );
    git_cmd(grit_dir.path(), &["commit", "-qm", "moved"]);
    // Make the physical mode agree with the moved HEAD's executable index entry.
    git_cmd(grit_dir.path(), &["reset", "--hard", "HEAD"]);
    std::fs::write(grit_dir.path().join("d029/f099"), "untouched\n").expect("local");
    let git_dir = tempfile::tempdir().expect("git");
    copy_repo(grit_dir.path(), git_dir.path());
    let outcome = open_grit(grit_dir.path())
        .stash()
        .pop(0, &StashApplyOptions::default())
        .expect("pop");
    assert_eq!(outcome.conflicted_paths, ["d000/f000"]);
    let mut cmd = Command::new("git");
    grit_test_support::configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(git_dir.path())
        .args(["stash", "pop"])
        .output()
        .expect("git pop");
    assert!(!out.status.success(), "Git must report the conflict");
    for args in [
        &["ls-files", "--stage"][..],
        &["status", "--porcelain"][..],
        &["stash", "list"][..],
    ] {
        assert_eq!(
            git_cmd(grit_dir.path(), args),
            git_cmd(git_dir.path(), args)
        );
    }
    for root in [grit_dir.path(), git_dir.path()] {
        assert_eq!(
            std::fs::read(root.join("d029/f099")).expect("local"),
            b"untouched\n"
        );
        assert_fsck_strict(root);
    }
    assert_eq!(
        std::fs::read(grit_dir.path().join("d000/f000")).expect("grit conflict"),
        std::fs::read(git_dir.path().join("d000/f000")).expect("git conflict")
    );
}
