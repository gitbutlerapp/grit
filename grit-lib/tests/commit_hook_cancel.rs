//! Pre-commit hooks honour [`Cancel`] and release index locks (issue #2412).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::thread;
use std::time::Duration;

use grit_lib::error::Error;
use grit_lib::ident::Signature;
use grit_lib::porcelain::commit::{commit_worktree, CommitRequest};
use grit_lib::progress::{FlagCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::rev_parse::resolve_revision_for_verify;

fn commit_req(message: &str) -> CommitRequest {
    let id = "Tester <t@e.test> 1700000000 +0000";
    let sig: Signature = id.parse().expect("signature");
    let mut req = CommitRequest::new(message);
    req.author = Some(sig.clone());
    req.committer = Some(sig);
    req
}

fn write_sleep_pre_commit(repo: &Repository, seconds: u32) {
    let hook = repo.git_dir().join("hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir");
    std::fs::write(hook, format!("#!/bin/sh\nsleep {seconds}\n")).expect("hook");
    std::fs::set_permissions(
        repo.git_dir().join("hooks/pre-commit"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
}

fn git_dir_lock_names(repo: &Repository) -> Vec<String> {
    std::fs::read_dir(repo.git_dir())
        .expect("read .git")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".lock"))
        .collect()
}

#[test]
fn cancelled_pre_commit_hook_leaves_no_index_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    std::fs::write(dir.path().join("a"), "base\n").expect("write");
    commit_worktree(
        &repo,
        &commit_req("base"),
        &mut NullProgress,
        &FlagCancel::new(),
    )
    .expect("seed commit");
    let head_before = resolve_revision_for_verify(&repo, "HEAD").expect("head");

    write_sleep_pre_commit(&repo, 60);
    std::fs::write(dir.path().join("a"), "dirty\n").expect("dirty");

    let cancel = FlagCancel::new();
    let cancel_worker = cancel.clone();
    let kick = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        cancel_worker.cancel();
    });
    let result = commit_worktree(&repo, &commit_req("two"), &mut NullProgress, &cancel);
    kick.join().expect("join");
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");

    let head_after = resolve_revision_for_verify(&repo, "HEAD").expect("head");
    assert_eq!(head_before, head_after, "HEAD must not move on cancel");

    assert!(
        git_dir_lock_names(&repo).is_empty(),
        "locks left behind: {:?}",
        git_dir_lock_names(&repo)
    );
}

#[test]
fn cancelled_pre_commit_hook_waits_for_child_to_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        dir.path(),
        &InitOptions::default().with_initial_branch("main".to_owned()),
    )
    .expect("init");
    std::fs::write(dir.path().join("a"), "base\n").expect("write");
    commit_worktree(
        &repo,
        &commit_req("base"),
        &mut NullProgress,
        &FlagCancel::new(),
    )
    .expect("seed commit");

    write_sleep_pre_commit(&repo, 2);
    std::fs::write(dir.path().join("a"), "dirty\n").expect("dirty");

    let cancel = FlagCancel::new();
    let cancel_worker = cancel.clone();
    let kick = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        cancel_worker.cancel();
    });
    let result = commit_worktree(&repo, &commit_req("two"), &mut NullProgress, &cancel);
    kick.join().expect("join");
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(git_dir_lock_names(&repo).is_empty());
}
