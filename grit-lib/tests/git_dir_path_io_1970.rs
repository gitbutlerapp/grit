//! Git-directory path I/O errors name `.git/…` paths (issue #1970).

use grit_lib::error::{Error, GitDirPathOperation};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, create_commit, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::repo::{InitOptions, Repository};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::tempdir;

fn chmod_tree_readonly(path: &Path) {
    let meta = fs::metadata(path).expect("metadata");
    let mut perms = meta.permissions();
    perms.set_mode(meta.permissions().mode() & !0o222);
    fs::set_permissions(path, perms).expect("chmod");
    if meta.is_dir() {
        for entry in fs::read_dir(path).expect("read_dir") {
            chmod_tree_readonly(&entry.expect("entry").path());
        }
    }
}

fn restore_tree_write(path: &Path) {
    let meta = fs::metadata(path).expect("metadata");
    let mut perms = meta.permissions();
    perms.set_mode(meta.permissions().mode() | 0o222);
    fs::set_permissions(path, perms).expect("chmod");
    if meta.is_dir() {
        for entry in fs::read_dir(path).expect("read_dir") {
            restore_tree_write(&entry.expect("entry").path());
        }
    }
}

fn assert_git_dir_io(err: &Error) {
    let msg = err.to_string();
    assert!(msg.contains(".git/"), "expected .git path in {msg}");
    assert!(
        !msg.starts_with("I/O error:"),
        "expected contextual git-dir message, got {msg}"
    );
}

fn commit_request(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.into(),
        author: Some("T <t@e.com> 1 +0000".parse().expect("signature")),
        committer: Some("T <t@e.com> 1 +0000".parse().expect("signature")),
        ..Default::default()
    }
}

fn init_repo(root: &Path) -> Repository {
    let repo = Repository::init(root, &InitOptions::new()).expect("init");
    fs::write(
        repo.git_dir().join("config"),
        "[user]\n\tname = T\n\temail = t@e.com\n",
    )
    .expect("config");
    repo
}

fn seed_commit(repo: &Repository, root: &Path, message: &str) {
    fs::write(root.join("a"), format!("{message}\n")).expect("write");
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    create_commit(repo, &commit_request(message), &mut NullProgress).expect("commit");
}

fn commit_should_fail_with_git_dir_io(repo: &Repository, root: &Path) -> Error {
    fs::write(root.join("a"), "change\n").expect("write");
    stage(repo, &StageOptions::default(), &mut NullProgress).expect("stage");
    commit_with_scope(
        repo,
        &commit_request("next"),
        CommitScope::Staged,
        &mut NullProgress,
    )
    .expect_err("commit should fail")
}

#[test]
fn read_only_objects_on_commit_names_git_path() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    seed_commit(&repo, dir.path(), "base");

    let objects = repo.git_dir().join("objects");
    chmod_tree_readonly(&objects);
    fs::write(dir.path().join("a"), "change\n").expect("write");
    let err = stage(&repo, &StageOptions::default(), &mut NullProgress).expect_err("stage");
    assert_git_dir_io(&err);
    restore_tree_write(&objects);
}

#[test]
fn read_only_refs_on_commit_names_git_path() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    seed_commit(&repo, dir.path(), "base");

    let heads = repo.git_dir().join("refs/heads");
    chmod_tree_readonly(&heads);
    let err = commit_should_fail_with_git_dir_io(&repo, dir.path());
    assert_git_dir_io(&err);
    restore_tree_write(&heads);
}

#[test]
fn read_only_logs_on_commit_names_git_path() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    seed_commit(&repo, dir.path(), "base");

    let logs = repo.git_dir().join("logs");
    chmod_tree_readonly(&logs);
    let err = commit_should_fail_with_git_dir_io(&repo, dir.path());
    assert_git_dir_io(&err);
    restore_tree_write(&logs);
}

#[test]
fn git_dir_path_io_variant_roundtrip() {
    let source = io::Error::from_raw_os_error(libc::EACCES as i32);
    let err = Error::GitDirPathIo {
        path: ".git/objects".to_owned(),
        operation: GitDirPathOperation::WriteObject,
        source,
    };
    assert_git_dir_io(&err);
}
