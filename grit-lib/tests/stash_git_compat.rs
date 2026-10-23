//! Round-trip stash create/list/pop/drop with system `git`.

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::stash::{
    apply_stash, create_stash, drop_stash, list_stashes, pop_stash, push_stash, stash_diff,
    StashCreateOptions,
};
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::refs::reflog::read_reflog;
use grit_lib::repo::Repository;
use grit_lib::worktree::{add_linked_worktree, AddLinkedWorktreeOptions};
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

fn stash_options(include_untracked: bool, message: Option<&str>) -> StashCreateOptions {
    StashCreateOptions {
        message: message.map(str::to_owned),
        include_untracked,
        identity: "Test <t@example.com> 1000000000 +0000".to_owned(),
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
    let oid = push_stash(&repo, &stash_options(false, Some("custom msg")))
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
    push_stash(&repo, &stash_options(true, None))
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
    let listed = list_stashes(&repo).expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].index, 0);
    assert!(listed[0].message.contains("second") || listed[1].message.contains("second"));

    pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000").expect("pop newest");

    let git_list = git_cmd(dir.path(), &["stash", "list"]);
    assert!(
        git_list.contains("first") && !git_list.contains("second"),
        "after pop, only older stash remains:\n{git_list}"
    );

    drop_stash(&repo, 0, "Test <t@example.com> 0 +0000").expect("drop last");
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
    push_stash(&repo, &stash_options(false, None))
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

    drop_stash(
        &open_grit(grit_dir.path()),
        2,
        "Test <t@example.com> 0 +0000",
    )
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

    drop_stash(
        &open_grit(grit_dir.path()),
        1,
        "Test <t@example.com> 0 +0000",
    )
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
    let none = create_stash(&repo, &stash_options(false, None)).expect("create");
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
    let conflicts = pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000")
        .expect("pop with conflict");
    assert!(conflicts, "three-way conflict should keep stash entry");
    assert_eq!(list_stashes(&repo).expect("list").len(), 1);
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

    pop_stash(
        &open_grit(grit_dir.path()),
        grit_dir.path(),
        0,
        true,
        "Test <t@example.com> 0 +0000",
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

    pop_stash(
        &open_grit(grit_dir.path()),
        grit_dir.path(),
        0,
        false,
        "Test <t@example.com> 0 +0000",
    )
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
    push_stash(&repo, &stash_options(true, Some("m")))
        .expect("push")
        .expect("stash");
    std::fs::write(dir.path().join("u.txt"), "PRECIOUS new work\n").expect("replace");

    let err = pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000")
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
    assert_eq!(list_stashes(&repo).expect("list").len(), 1);

    let err = apply_stash(
        &repo,
        dir.path(),
        &list_stashes(&repo).expect("list")[0].oid,
        false,
        "Test <t@example.com> 0 +0000",
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
    push_stash(&repo, &stash_options(true, None))
        .expect("push")
        .expect("stash");
    std::fs::write(dir.path().join(".gitignore"), "u.txt\n").expect("ignore");
    std::fs::write(dir.path().join("u.txt"), "precious\n").expect("ignored precious");

    let err = apply_stash(
        &repo,
        dir.path(),
        &list_stashes(&repo).expect("list")[0].oid,
        false,
        "Test <t@example.com> 0 +0000",
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
    push_stash(&repo, &stash_options(true, None))
        .expect("push")
        .expect("stash");
    std::fs::write(dir.path().join("u.txt"), "precious\n").expect("precious");
    git_cmd(dir.path(), &["add", "u.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "track u"]);

    let err = apply_stash(
        &repo,
        dir.path(),
        &list_stashes(&repo).expect("list")[0].oid,
        false,
        "Test <t@example.com> 0 +0000",
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
    push_stash(&repo, &stash_options(false, None))
        .expect("push")
        .expect("oid");

    let diffs = stash_diff(&repo, 0).expect("diff");
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
    let oid = push_stash(&repo, &stash_options(false, Some("grit msg")))
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
    apply_stash(
        &open_grit(grit_dir.path()),
        grit_dir.path(),
        &stash_oid,
        true,
        "Test <t@example.com> 0 +0000",
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

    push_stash(&open_grit(grit_root.path()), &stash_options(true, None))
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

    push_stash(
        &open_grit(grit_root.path()),
        &stash_options(true, Some("with-u")),
    )
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
            branch: Some("linked".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("add worktree");

    std::fs::write(linked.join("f"), "linked change\n").expect("modify");
    push_stash(
        &Repository::discover(Some(linked.as_path()), &RepositoryOptions::empty())
            .expect("discover linked"),
        &stash_options(false, Some("from-linked")),
    )
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
    push_stash(&repo, &stash_options(true, None))
        .expect("push")
        .expect("stash oid");

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push -u must leave a clean tree like git stash, got:\n{clean}"
    );
    assert!(dir.path().join("d/c.txt").is_file());
    assert!(dir.path().join("d/e/f.txt").is_file());

    pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000").expect("pop");

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
    let stash_oid = push_stash(&repo, &stash_options(true, None))
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

    let err = pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000")
        .expect_err("pop must fail like git when untracked dir cannot be restored");
    assert!(
        matches!(err, Error::StashCouldNotRestoreUntracked { .. }),
        "unexpected error: {err:?}"
    );
    assert_eq!(list_stashes(&repo).expect("list").len(), 1);
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
    push_stash(&repo, &stash_options(false, None))
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

    pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000").expect("pop");

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
    push_stash(&repo, &stash_options(false, None))
        .expect("push")
        .expect("stash");

    let clean = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        clean.trim().is_empty(),
        "push_stash must leave clean tree for nested path, got:\n{clean}"
    );

    pop_stash(&repo, dir.path(), 0, false, "Test <t@example.com> 0 +0000").expect("pop");
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
    push_stash(&repo, &stash_options(false, None))
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
    push_stash(&repo, &stash_options(true, None))
        .expect("push")
        .expect("had changes");

    let stash_blob = git_cmd(dir.path(), &["cat-file", "-p", "stash^3:new.txt"]);
    assert_eq!(
        stash_blob, "n\n",
        "untracked stash parent must LF-normalize under eol=crlf"
    );
    assert_fsck_strict(dir.path());
}
