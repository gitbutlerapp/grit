//! Held `index.lock`: typed [`Error::LockHeld`] with the pid sidecar's live vs stale holder
//! (native liveness, no `kill` CLI), and Git sees the repository unchanged afterwards.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::IndexLock;
use grit_lib::lockfile::{grit_lockfile_pid_enabled, pid_path_for_lock, LockHolder};
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
        Error::LockHeld(grit_lib::error::LockHeldError { path, holder }) => (path, holder),
        other => panic!("expected Error::LockHeld, got {other:?}"),
    }
}

#[test]
fn index_lock_acquire_writes_pid_sidecar_by_default() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let index_path = tmp.path().join("index");
    fs::write(&index_path, b"").expect("touch index");
    let lock_path = index_path.with_extension("lock");
    let lock = IndexLock::acquire(&index_path, None).expect("acquire");
    let pid_path = pid_path_for_lock(&lock_path);
    assert_eq!(
        fs::read_to_string(&pid_path).expect("sidecar"),
        format!("pid {}\n", std::process::id())
    );
    drop(lock);
    assert!(!lock_path.exists());
    assert!(!pid_path.exists());
}

#[test]
fn index_lock_acquire_skips_pid_sidecar_when_lockfile_pid_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let git_dir = tmp.path();
    fs::write(git_dir.join("config"), "[core]\n\tlockfilePid = false\n").expect("config");
    let index_path = git_dir.join("index");
    fs::write(&index_path, b"").expect("touch index");
    assert!(!grit_lockfile_pid_enabled(None, Some(git_dir)));
    let lock = IndexLock::acquire(&index_path, None).expect("acquire");
    assert!(!pid_path_for_lock(&index_path.with_extension("lock")).exists());
    drop(lock);
}

#[test]
fn lock_holder_remove_if_stale_unlinks_dead_holder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_path = tmp.path().join("index.lock");
    hold_lock(&lock_path, Some(DEAD_PID));
    assert!(LockHolder::remove_if_stale(&lock_path).expect("remove"));
    assert!(!lock_path.exists());
    assert!(!tmp.path().join("index~pid.lock").exists());
}

#[test]
fn lock_holder_remove_if_stale_keeps_live_holder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_path = tmp.path().join("index.lock");
    hold_lock(&lock_path, Some(u64::from(std::process::id())));
    assert!(!LockHolder::remove_if_stale(&lock_path).expect("probe"));
    assert!(lock_path.exists());
}

#[test]
fn lock_holder_remove_if_stale_without_sidecar_is_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let lock_path = tmp.path().join("index.lock");
    hold_lock(&lock_path, None);
    assert!(!LockHolder::remove_if_stale(&lock_path).expect("probe"));
    assert!(lock_path.exists());
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
    for pid in [None, Some(u64::from(std::process::id())), Some(DEAD_PID)] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        git(wt, &["init", "-q", "-b", "main"]);
        fs::write(wt.join("n"), "n\n").expect("write n");
        let lock_path = wt.join(".git/index.lock");
        hold_lock(&lock_path, pid);

        let repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
        let options = StageOptions {
            pathspecs: vec!["n".into()],
            ..Default::default()
        };
        let err = stage(
            &repo,
            &options,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect_err("index.lock is held");
        let (path, holder) = expect_lock_held(err);
        assert_eq!(
            path.canonicalize().expect("lock path"),
            lock_path.canonicalize().expect("lock path")
        );
        assert_eq!(
            holder,
            pid.map(|pid| LockHolder {
                pid,
                running: pid != DEAD_PID,
            })
        );
        assert_eq!(fs::read(&lock_path).expect("foreign lock kept"), b"");
        assert!(!wt.join(".git/index").exists(), "nothing was staged");
        if let Some(pid) = pid {
            assert_eq!(
                fs::read_to_string(wt.join(".git/index~pid.lock")).expect("foreign pid kept"),
                format!("pid {pid}\n")
            );
            fs::remove_file(wt.join(".git/index~pid.lock")).expect("release pid sidecar");
        }

        fs::remove_file(&lock_path).expect("release lock for git");
        assert_eq!(git(wt, &["status", "--porcelain"]), "?? n\n");
        stage(
            &repo,
            &options,
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("retry after lock release");
        assert_eq!(git(wt, &["show", ":n"]), "n\n");
        git(wt, &["fsck", "--strict"]);

        fs::write(wt.join("n"), "written by git\n").expect("modify n");
        git(wt, &["add", "n"]);
        let index = repo.index().load().expect("read Git-written index");
        let entry = index.get(b"n", 0).expect("Git-staged path");
        assert_eq!(entry.oid.to_hex(), git(wt, &["rev-parse", ":n"]).trim());
    }
}
