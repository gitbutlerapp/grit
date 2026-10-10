//! Round-trip stash create/list/pop/drop with system `git`.

use grit_lib::porcelain::stash::{
    create_stash, drop_stash, list_stashes, pop_stash, push_stash, stash_diff, StashCreateOptions,
};
use grit_lib::reflog::read_reflog;
use grit_lib::repo::Repository;
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
    Repository::open(&root.join(".git"), Some(root)).expect("open grit repo")
}

fn stash_options(include_untracked: bool, message: Option<&str>) -> StashCreateOptions {
    StashCreateOptions {
        message: message.map(str::to_owned),
        include_untracked,
        identity: "Test <t@example.com> 1000000000 +0000".to_owned(),
    }
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
        .expect("had changes");

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
