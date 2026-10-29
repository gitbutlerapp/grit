//! Git-directory path I/O errors name `.git/…` paths (issue #1970).

#![cfg(unix)]

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::{Error, GitDirPathOperation};
use grit_lib::maintenance::{gc, GcOptions};
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, create_commit, CommitRequest, CommitScope};
use grit_lib::progress::NullProgress;
use grit_lib::refs::{write_ref, write_ref_cas};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, AddLinkedWorktreeOptions, WorktreeBranch,
};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::tempdir;
use time::OffsetDateTime;

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

fn assert_git_dir_path_io(err: &Error) -> String {
    match err {
        Error::GitDirPathIo { path, .. } => {
            assert_git_dir_io(err);
            path.clone()
        }
        other => panic!("expected GitDirPathIo, got {other}"),
    }
}

fn chmod_dir_readonly(path: &Path) {
    let meta = fs::metadata(path).expect("metadata");
    let mut perms = meta.permissions();
    perms.set_mode(meta.permissions().mode() & !0o222);
    fs::set_permissions(path, perms).expect("chmod");
}

fn restore_dir_write(path: &Path) {
    let meta = fs::metadata(path).expect("metadata");
    let mut perms = meta.permissions();
    perms.set_mode(meta.permissions().mode() | 0o222);
    fs::set_permissions(path, perms).expect("chmod");
}

fn commit_request(message: &str) -> CommitRequest {
    grit_test_support::commit_request(message, "T <t@e.com> 1 +0000")
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
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");
    create_commit(
        repo,
        &commit_request(message),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("commit");
}

fn commit_should_fail_with_git_dir_io(repo: &Repository, root: &Path) -> Error {
    fs::write(root.join("a"), "change\n").expect("write");
    stage(
        repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");
    commit_with_scope(
        repo,
        &commit_request("next"),
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
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
    let err = stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("stage");
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
fn read_only_existing_pack_dir_on_gc_names_git_path() {
    let dir = tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    seed_commit(&repo, dir.path(), "base");
    let now = OffsetDateTime::now_utc().unix_timestamp();
    gc(&repo, &GcOptions::full_run(false, now)).expect("initial gc creates pack");
    let pack_dir = repo.git_dir().join("objects/pack");
    assert!(pack_dir.is_dir(), "expected pack directory after gc");

    seed_commit(&repo, dir.path(), "more");
    chmod_tree_readonly(&pack_dir);
    let err = gc(&repo, &GcOptions::full_run(false, now)).expect_err("gc should fail");
    match &err {
        Error::GitDirPathIo {
            path,
            operation: GitDirPathOperation::GcPack,
            ..
        } => {
            assert!(
                path.contains("objects/pack"),
                "expected pack path in {path}"
            );
            assert!(path.contains(".git/") || path.starts_with("objects/pack"));
        }
        other => panic!("expected GitDirPathIo, got {other}"),
    }
    assert_git_dir_io(&err);
    restore_tree_write(&pack_dir);
}

#[test]
fn read_only_linked_worktree_metadata_on_detached_commit_names_head_lock() {
    let dir = tempdir().expect("tempdir");
    let main_wt = dir.path().join("main");
    fs::create_dir_all(&main_wt).expect("main wt");
    let repo = Repository::init(&main_wt, &InitOptions::new()).expect("init");
    fs::write(
        repo.git_dir().join("config"),
        "[user]\n\tname = T\n\temail = t@e.com\n",
    )
    .expect("config");
    seed_commit(&repo, &main_wt, "base");

    let linked_wt = dir.path().join("linked");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: linked_wt.clone(),
            branch: None,
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("add linked worktree");

    let linked_repo = Repository::discover(Some(linked_wt.as_path()), &RepositoryOptions::empty())
        .expect("discover linked");
    let head_oid = match resolve_head(linked_repo.git_dir()).expect("head") {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        other => panic!("unexpected head state: {other:?}"),
    };
    write_ref(linked_repo.git_dir(), "HEAD", &head_oid).expect("detach head");

    let admin = linked_repo.git_dir().to_path_buf();
    chmod_tree_readonly(&admin);

    let err = write_ref_cas(linked_repo.git_dir(), "HEAD", &head_oid, head_oid)
        .expect_err("HEAD update should fail");
    match &err {
        Error::GitDirPathIo { path, .. } => {
            assert!(
                path.contains(".git/worktrees/") && path.contains("HEAD.lock"),
                "expected linked worktree HEAD.lock path, got {path}"
            );
        }
        other => panic!("expected GitDirPathIo, got {other}"),
    }
    restore_tree_write(&admin);
}

#[test]
fn linked_worktree_commit_readonly_admin_names_git_path() {
    let dir = tempdir().expect("tempdir");
    let main_wt = dir.path().join("main");
    fs::create_dir_all(&main_wt).expect("main wt");
    let repo = init_repo(&main_wt);
    seed_commit(&repo, &main_wt, "base");

    let linked_wt = dir.path().join("linked");
    add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path: linked_wt.clone(),
            branch: Some(WorktreeBranch::Detach),
            start_point: Some("HEAD".into()),
            force: false,
            lock: None,
            no_checkout: false,
            track: false,
        },
    )
    .expect("add linked worktree");

    let linked_repo = Repository::discover(Some(linked_wt.as_path()), &RepositoryOptions::empty())
        .expect("discover linked");
    fs::write(linked_wt.join("a"), "linked change\n").expect("write");
    stage(
        &linked_repo,
        &StageOptions::default(),
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");

    let admin = list_worktrees(&repo)
        .expect("list")
        .into_iter()
        .find(|e| e.path == linked_wt.canonicalize().unwrap_or(linked_wt.clone()))
        .expect("linked entry")
        .admin_dir;
    chmod_dir_readonly(&admin);

    let err = commit_with_scope(
        &linked_repo,
        &commit_request("linked"),
        CommitScope::Staged,
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect_err("commit should fail");
    let path = assert_git_dir_path_io(&err);
    assert!(
        path.contains(".git/worktrees/") && path.contains("index"),
        "expected linked worktree index path, got {path}"
    );

    restore_dir_write(&admin);
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
