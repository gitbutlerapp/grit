//! Auto-staging must not commit untracked paths hidden by `status.showUntrackedFiles=no` (issue #2523).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

fn commit_req(message: &str) -> CommitRequest {
    let id = "Untracked No <hide@example.com> 1700000000 +0000";
    let mut req = CommitRequest::new(message.to_owned());
    req.author = Some(id.parse().expect("signature"));
    req.committer = Some(id.parse().expect("signature"));
    req
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Untracked No")
        .env("GIT_AUTHOR_EMAIL", "hide@example.com")
        .env("GIT_COMMITTER_NAME", "Untracked No")
        .env("GIT_COMMITTER_EMAIL", "hide@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Untracked No")
        .env("GIT_AUTHOR_EMAIL", "hide@example.com")
        .env("GIT_COMMITTER_NAME", "Untracked No")
        .env("GIT_COMMITTER_EMAIL", "hide@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn set_show_untracked_files(root: &Path, value: &str) {
    if value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("normal")
        || value.eq_ignore_ascii_case("all")
    {
        git(root, &["config", "status.showUntrackedFiles", value]);
        return;
    }
    // Git's `git config` rejects boolean synonyms for this key; grit reads them from the file.
    let config_path = root.join(".git/config");
    let mut contents = std::fs::read_to_string(&config_path).expect("read config");
    contents.push_str("[status]\n\tshowUntrackedFiles = ");
    contents.push_str(value);
    contents.push('\n');
    std::fs::write(&config_path, contents).expect("write config");
}

fn init_repo_with_untracked_hidden(root: &Path, show_untracked: &str) {
    git(root, &["init", "-q", "-b", "main"]);
    let defer_git_incompatible_config =
        show_untracked.eq_ignore_ascii_case("false") || show_untracked == "0";
    if !defer_git_incompatible_config {
        set_show_untracked_files(root, show_untracked);
    }
    std::fs::write(root.join("tracked"), "1\n").expect("write tracked");
    git(root, &["add", "tracked"]);
    git(root, &["commit", "-qm", "init"]);
    if defer_git_incompatible_config {
        set_show_untracked_files(root, show_untracked);
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("tracked"))
        .expect("append tracked")
        .write_all(b"2\n")
        .expect("write");
    std::fs::write(root.join(".env"), "secret\n").expect("write .env");
    std::fs::write(root.join("id_rsa"), "key\n").expect("write id_rsa");
}

#[test]
fn autostage_commit_skips_hidden_untracked_no() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_untracked_hidden(root, "no");

    commit_with_scope(
        &open_grit(root),
        &commit_req("tracked change"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    let stat = git_out(root, &["show", "--stat", "--format=%s", "HEAD"]);
    assert!(stat.contains("tracked change"), "subject missing: {stat}");
    assert!(
        !stat.contains(".env") && !stat.contains("id_rsa"),
        "hidden untracked must not be committed: {stat}"
    );
    assert!(stat.contains("tracked"), "tracked edit must commit: {stat}");
    git(root, &["fsck", "--strict"]);
}

#[test]
fn autostage_commit_skips_hidden_untracked_false_synonym() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_untracked_hidden(root, "false");

    commit_with_scope(
        &open_grit(root),
        &commit_req("tracked change"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    let names = git_out(
        root,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    );
    assert_eq!(names.trim(), "tracked");
}

#[test]
fn autostage_commit_still_includes_untracked_when_show_normal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    init_repo_with_untracked_hidden(root, "normal");

    commit_with_scope(
        &open_grit(root),
        &commit_req("everything"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    let names = git_out(
        root,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    );
    assert!(
        names.contains(".env"),
        "normal mode should stage untracked: {names}"
    );
    assert!(names.contains("id_rsa"), "{names}");
    assert!(names.contains("tracked"), "{names}");
}

#[test]
fn dotfiles_git_dir_worktree_does_not_sweep_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let bare = root.join("cfg.git");
    git(root, &["init", "--bare", "-q", "cfg.git"]);
    git(
        root,
        &[
            "--git-dir",
            bare.to_str().expect("path"),
            "config",
            "status.showUntrackedFiles",
            "no",
        ],
    );
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::write(home.join(".bashrc"), "shell\n").expect("bashrc");
    std::fs::write(home.join(".env"), "secret\n").expect("env");
    std::fs::write(home.join("id_rsa"), "key\n").expect("key");

    let repo = Repository::open(&bare, &RepositoryOptions::empty().with_work_tree(&home))
        .expect("open dotfiles repo");

    grit_lib::porcelain::add::stage(
        &repo,
        &grit_lib::porcelain::add::StageOptions {
            pathspecs: vec![".bashrc".to_owned()],
            pathspec_sources: vec![".bashrc".to_owned()],
            ..Default::default()
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage bashrc");

    let outcome = commit_with_scope(
        &repo,
        &commit_req("dotfiles"),
        CommitScope::AutoStage,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");

    let names = Command::new("git")
        .current_dir(&home)
        .args([
            "--git-dir",
            bare.to_str().expect("path"),
            "ls-tree",
            "-r",
            "--name-only",
            &outcome.oid.to_hex(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git diff-tree");
    assert!(names.status.success());
    let names = String::from_utf8_lossy(&names.stdout);
    assert_eq!(
        names.trim(),
        ".bashrc",
        "only staged path should commit: {names}"
    );
}
