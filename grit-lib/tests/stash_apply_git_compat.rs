//! `apply_stash` parity with system `git stash apply` (porcelain + file bytes).

use grit_lib::objects::ObjectId;
use grit_lib::porcelain::stash::apply_stash;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn copy_repo(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir dest");
    let status = std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", from.display()),
            &to.to_string_lossy(),
        ])
        .status()
        .expect("cp");
    assert!(status.success(), "cp -a failed");
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

#[test]
fn apply_stash_without_index_matches_git_when_head_unchanged() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("x"), "committed\n").expect("write");
    git_cmd(base.path(), &["add", "x"]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    let head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    std::fs::write(base.path().join("x"), "stashed content\n").expect("modify");
    git_cmd(base.path(), &["stash", "push", "-qm", "bench"]);
    let stash_oid_hex = git_cmd(base.path(), &["rev-parse", "refs/stash"])
        .trim()
        .to_owned();
    git_cmd(base.path(), &["reset", "--hard", &head]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo =
        Repository::open(&grit_dir.path().join(".git"), Some(grit_dir.path())).expect("open");
    let stash_oid = ObjectId::from_hex(&stash_oid_hex).expect("stash oid");
    apply_stash(&grit_repo, grit_dir.path(), &stash_oid, false, false).expect("apply");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git_cmd(git_dir.path(), &["stash", "apply", "stash@{0}"]);

    let grit_porcelain = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(
        grit_porcelain, git_porcelain,
        "grit and git porcelain must match for apply without --index"
    );
    assert!(
        grit_porcelain.contains("x"),
        "expected x in porcelain (git shows unstaged modify), got:\n{grit_porcelain}"
    );

    let grit_bytes = std::fs::read(grit_dir.path().join("x")).expect("read grit x");
    let git_bytes = std::fs::read(git_dir.path().join("x")).expect("read git x");
    assert_eq!(grit_bytes, git_bytes);
    assert_eq!(grit_bytes, b"stashed content\n");
}

#[test]
fn apply_stash_with_index_matches_git_staged_porcelain() {
    let base = tempfile::tempdir().expect("tempdir");
    init_repo(base.path());
    std::fs::write(base.path().join("y"), "base\n").expect("write");
    git_cmd(base.path(), &["add", "y"]);
    git_cmd(base.path(), &["commit", "-qm", "init"]);
    let head = git_cmd(base.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();

    std::fs::write(base.path().join("y"), "staged stash\n").expect("modify");
    git_cmd(base.path(), &["add", "y"]);
    git_cmd(base.path(), &["stash", "push", "-qm", "bench"]);
    let stash_oid_hex = git_cmd(base.path(), &["rev-parse", "refs/stash"])
        .trim()
        .to_owned();
    git_cmd(base.path(), &["reset", "--hard", &head]);

    let grit_dir = tempfile::tempdir().expect("grit copy");
    copy_repo(base.path(), grit_dir.path());
    let grit_repo =
        Repository::open(&grit_dir.path().join(".git"), Some(grit_dir.path())).expect("open");
    apply_stash(
        &grit_repo,
        grit_dir.path(),
        &ObjectId::from_hex(&stash_oid_hex).expect("stash"),
        true,
        false,
    )
    .expect("apply");

    let git_dir = tempfile::tempdir().expect("git copy");
    copy_repo(base.path(), git_dir.path());
    git_cmd(git_dir.path(), &["stash", "apply", "--index", "stash@{0}"]);

    let grit_porcelain = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(grit_porcelain, git_porcelain);
    assert!(
        !grit_porcelain.trim().is_empty(),
        "staged stash apply should leave staged entries in porcelain"
    );
    assert_eq!(
        std::fs::read(grit_dir.path().join("y")).expect("grit y"),
        b"staged stash\n"
    );
}
