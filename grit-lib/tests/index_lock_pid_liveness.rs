//! Held `index.lock`: typed [`Error::LockHeld`] with the pid sidecar's live vs stale holder
//! (native liveness, no `kill` CLI), and Git sees the repository unchanged afterwards.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::IndexLock;
use grit_lib::lockfile::LockHolder;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;

/// A pid above every platform's pid limit, so no process can be running with it.
const DEAD_PID: u64 = 999_999_999;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
    String::from_utf8(out.stdout).expect("utf8")
}

fn hold_lock(lock_path: &Path, pid: Option<u64>) {
    fs::write(lock_path, b"").expect("touch lock");
    if let Some(pid) = pid {
        let pid_path = lock_path.with_file_name("index~pid.lock");
        fs::write(pid_path, format!("pid {pid}\n")).expect("write pid");
    }
}

fn expect_lock_held(err: Error) -> (std::path::PathBuf, Option<LockHolder>) {
    match err {
        Error::LockHeld { path, holder } => (path, holder),
        other => panic!("expected Error::LockHeld, got {other:?}"),
    }
}

#[test]
fn lock_holder_reports_live_pid_for_current_process() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_path = tmp.path().join("index.lock");
    hold_lock(&lock_path, Some(u64::from(std::process::id())));

    assert_eq!(
        LockHolder::read(&lock_path),
        Some(LockHolder {
            pid: u64::from(std::process::id()),
            running: true,
        })
    );
}

#[test]
fn lock_holder_reports_stale_pid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_path = tmp.path().join("index.lock");
    hold_lock(&lock_path, Some(DEAD_PID));

    assert_eq!(
        LockHolder::read(&lock_path),
        Some(LockHolder {
            pid: DEAD_PID,
            running: false,
        })
    );
}

#[test]
fn lock_holder_absent_without_pid_sidecar() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_path = tmp.path().join("index.lock");
    hold_lock(&lock_path, None);
    assert_eq!(LockHolder::read(&lock_path), None);

    fs::write(tmp.path().join("index~pid.lock"), "not a pid\n").expect("write garbage");
    assert_eq!(LockHolder::read(&lock_path), None);
}

#[test]
fn index_lock_acquire_reports_typed_lock_held_with_holder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let index_path = tmp.path().join("index");
    let lock_path = tmp.path().join("index.lock");

    hold_lock(&lock_path, Some(u64::from(std::process::id())));
    let err = IndexLock::acquire(&index_path, None)
        .err()
        .expect("held lock");
    let (path, holder) = expect_lock_held(err);
    assert_eq!(path, lock_path);
    assert_eq!(
        holder,
        Some(LockHolder {
            pid: u64::from(std::process::id()),
            running: true,
        })
    );

    hold_lock(&lock_path, Some(DEAD_PID));
    let err = IndexLock::acquire(&index_path, None)
        .err()
        .expect("stale lock is still held");
    let (_, holder) = expect_lock_held(err);
    assert_eq!(holder.map(|h| h.running), Some(false));
    assert_eq!(
        fs::read(&lock_path).expect("foreign lock kept"),
        b"",
        "a foreign lock must never be removed"
    );
}

#[test]
fn stage_with_held_index_lock_returns_lock_held_and_git_sees_no_change() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path();
    git(wt, &["init", "-q", "-b", "main"]);
    fs::write(wt.join("n"), "n\n").expect("write n");
    let lock_path = wt.join(".git/index.lock");
    fs::write(&lock_path, b"").expect("touch index.lock");

    let repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    let options = StageOptions {
        pathspecs: vec!["n".into()],
        ..Default::default()
    };
    let err = stage(&repo, &options, &mut NullProgress).expect_err("index.lock is held");
    let (path, holder) = expect_lock_held(err);
    assert_eq!(
        path.canonicalize().expect("lock path"),
        lock_path.canonicalize().expect("lock path")
    );
    assert_eq!(holder, None);
    assert!(lock_path.is_file(), "the foreign lock must stay in place");

    fs::remove_file(&lock_path).expect("release lock for git");
    assert_eq!(git(wt, &["status", "--porcelain"]), "?? n\n");
    git(wt, &["fsck", "--strict"]);
}
