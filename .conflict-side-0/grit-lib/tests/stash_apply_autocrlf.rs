//! Stash apply/pop with `core.autocrlf=true` and CRLF worktree bytes (issue #958).

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::stash::{apply_stash, pop_stash, push_stash, StashCreateOptions};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn git_cmd(repo: &std::path::Path, args: &[&str]) -> String {
    git(repo, args)
}

fn init_repo(root: &std::path::Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@e.com"]);
    git_cmd(root, &["config", "user.name", "T"]);
    git_cmd(root, &["config", "core.autocrlf", "true"]);
}

fn stash_options() -> StashCreateOptions {
    StashCreateOptions {
        message: None,
        include_untracked: false,
        identity: "T <t@e.com> 1000000000 +0000".to_owned(),
    }
}

fn open_grit(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

/// After stash reset, simulate Git for Windows: LF blob in the index, CRLF on disk, clean status.
fn ensure_crlf_worktree_lf_index(root: &std::path::Path) {
    let path = root.join("a.txt");
    let crlf = b"one\r\ntwo\r\nthree\r\n";
    std::fs::write(&path, crlf).expect("crlf worktree");
    let porcelain = git_cmd(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "git must treat CRLF worktree as clean with autocrlf and LF index, got:\n{porcelain}"
    );
}

#[test]
fn apply_stash_succeeds_on_clean_tree_with_autocrlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\nfour\n").expect("modify");
    let repo = open_grit(dir.path());
    push_stash(&repo, &stash_options())
        .expect("push")
        .expect("had changes");

    ensure_crlf_worktree_lf_index(dir.path());

    let stash_oid = grit_lib::refs::resolve_ref(repo.git_dir(), "refs/stash").expect("stash ref");

    let grit_dir = tempfile::tempdir().expect("grit copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", dir.path().display()),
            &grit_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp")
        .success()
        .then_some(())
        .expect("cp failed");
    let grit_repo = open_grit(grit_dir.path());
    apply_stash(
        &grit_repo,
        grit_dir.path(),
        &stash_oid,
        false,
        "T <t@e.com> 0 +0000",
    )
    .expect("apply must not treat CRLF smudge as local changes");

    let git_dir = tempfile::tempdir().expect("git copy");
    std::process::Command::new("cp")
        .args([
            "-a",
            &format!("{}/.", dir.path().display()),
            &git_dir.path().to_string_lossy(),
        ])
        .status()
        .expect("cp")
        .success()
        .then_some(())
        .expect("cp failed");
    git_cmd(git_dir.path(), &["stash", "apply", "stash@{0}"]);

    let grit_porcelain = git_cmd(grit_dir.path(), &["status", "--porcelain"]);
    let git_porcelain = git_cmd(git_dir.path(), &["status", "--porcelain"]);
    assert_eq!(
        grit_porcelain, git_porcelain,
        "porcelain after apply must match git stash apply"
    );
    assert!(
        grit_porcelain.contains("a.txt"),
        "expected modified a.txt in porcelain, got:\n{grit_porcelain}"
    );
}

#[test]
fn pop_stash_succeeds_on_clean_tree_with_autocrlf() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\n").expect("write");
    git_cmd(dir.path(), &["add", "a.txt"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);

    std::fs::write(dir.path().join("a.txt"), b"one\ntwo\nthree\nfour\n").expect("modify");
    let repo = open_grit(dir.path());
    push_stash(&repo, &stash_options())
        .expect("push")
        .expect("had changes");

    ensure_crlf_worktree_lf_index(dir.path());

    pop_stash(&repo, dir.path(), 0, false, "T <t@e.com> 0 +0000").expect("pop");

    let list = git_cmd(dir.path(), &["stash", "list"]);
    assert!(
        list.trim().is_empty(),
        "pop must drop stash entry, got:\n{list}"
    );
    let porcelain = git_cmd(dir.path(), &["status", "--porcelain"]);
    assert!(
        porcelain.contains("a.txt"),
        "pop must restore stashed change, got:\n{porcelain}"
    );
}
