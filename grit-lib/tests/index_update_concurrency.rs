//! Concurrent index writers must not lose each other's updates (issue #1792).
//!
//! Every read-modify-write of `.git/index` (staging, removing, `commit -a` style tracked
//! staging) has to read the index while holding `index.lock`. These tests race grit writers
//! against each other and against an outside lock holder, then check the result with the
//! system `git`.

use crate::support;

use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use grit_lib::error::Error;
use grit_lib::porcelain::add::{stage, stage_on_index, StageOptions};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions};
use grit_lib::porcelain::stage_tracked::stage_tracked_modifications;
use grit_lib::progress::NullProgress;

use support::{assert_git_fsck_strict, git, git_ls_files_stage0_map, open_grit_repo};

/// How long the outside lock holder keeps `index.lock` so a racing writer is already
/// blocked (or, without the fix, has already read the stale index) when it lets go.
const HOLD_LOCK_FOR: Duration = Duration::from_millis(400);

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("base"), "x\n").expect("write base");
    git(dir, &["add", "base"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn stage_paths(paths: &[&str]) -> StageOptions {
    StageOptions {
        pathspecs: paths.iter().map(|p| (*p).to_owned()).collect(),
        ..StageOptions::default()
    }
}

fn is_lock_contention(err: &Error) -> bool {
    matches!(err, Error::LockHeld { .. })
}

/// Stage `path` into the on-disk index from outside grit's staging entry points, while the
/// caller already holds `index.lock`: what a concurrent `grit add` / `git add` would leave behind.
fn write_entry_under_lock(dir: &Path, path: &str, lock: &mut grit_lib::index::IndexLock) {
    let repo = open_grit_repo(dir);
    let handle = repo.index_handle();
    let mut index = handle.load().expect("load index under lock");
    stage_on_index(&repo, &mut index, &stage_paths(&[path]), &mut NullProgress)
        .expect("stage in memory");
    handle
        .commit_update(lock, &mut index, false)
        .expect("commit held lock");
}

fn run_parallel_stage(dir: &Path, writers: usize) -> Vec<(String, Result<(), Error>)> {
    for i in 0..writers {
        fs::write(dir.join(format!("n{i}.txt")), format!("{i}\n")).expect("write file");
    }
    let start = Arc::new(Barrier::new(writers));
    let handles: Vec<_> = (0..writers)
        .map(|i| {
            let dir = dir.to_path_buf();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let path = format!("n{i}.txt");
                let repo = open_grit_repo(&dir);
                start.wait();
                let res = stage(&repo, &stage_paths(&[&path]), &mut NullProgress).map(|_| ());
                (path, res)
            })
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().expect("writer thread"))
        .collect()
}

#[test]
fn parallel_stage_keeps_every_successful_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);

    let results = run_parallel_stage(dir, 24);
    let staged = git_ls_files_stage0_map(dir);
    let mut succeeded = 0;
    for (path, res) in &results {
        match res {
            Ok(()) => {
                succeeded += 1;
                assert!(
                    staged.contains_key(path),
                    "{path} was reported staged but is missing from the index"
                );
            }
            Err(err) => assert!(is_lock_contention(err), "{path}: unexpected error {err}"),
        }
    }
    assert!(succeeded > 0, "at least one writer must win the lock");
    assert_eq!(staged.len(), 1 + succeeded, "index: {staged:?}");
    assert_git_fsck_strict(dir);
}

#[test]
fn parallel_stage_with_unbounded_lock_wait_stages_everything() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    git(dir, &["config", "core.filesRefLockTimeout", "-1"]);

    let writers = 24;
    let results = run_parallel_stage(dir, writers);
    for (path, res) in &results {
        assert!(res.is_ok(), "{path}: {res:?}");
    }
    let staged = git_ls_files_stage0_map(dir);
    assert_eq!(staged.len(), 1 + writers, "index: {staged:?}");
    for i in 0..writers {
        assert!(staged.contains_key(&format!("n{i}.txt")));
    }
    assert_git_fsck_strict(dir);
}

#[test]
fn stage_reads_index_after_waiting_for_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    git(dir, &["config", "core.filesRefLockTimeout", "-1"]);
    fs::write(dir.join("a.txt"), "a\n").expect("write a");
    fs::write(dir.join("b.txt"), "b\n").expect("write b");

    let repo = open_grit_repo(dir);
    let mut lock = repo.index_handle().begin_update().expect("hold index.lock");

    let racer_dir = dir.to_path_buf();
    let racer = thread::spawn(move || {
        let repo = open_grit_repo(&racer_dir);
        stage(&repo, &stage_paths(&["b.txt"]), &mut NullProgress).map(|o| o.added)
    });
    thread::sleep(HOLD_LOCK_FOR);
    write_entry_under_lock(dir, "a.txt", &mut lock);
    drop(lock);

    let added = racer.join().expect("racer").expect("racing stage");
    assert_eq!(added, vec!["b.txt".to_owned()]);
    let staged = git_ls_files_stage0_map(dir);
    assert_eq!(
        staged.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a.txt", "b.txt", "base"],
        "the entry written while the lock was held must survive"
    );
    assert_git_fsck_strict(dir);
}

#[test]
fn remove_reads_index_after_waiting_for_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    git(dir, &["config", "core.filesRefLockTimeout", "-1"]);
    fs::write(dir.join("a.txt"), "a\n").expect("write a");

    let repo = open_grit_repo(dir);
    let mut lock = repo.index_handle().begin_update().expect("hold index.lock");

    let racer_dir = dir.to_path_buf();
    let racer = thread::spawn(move || {
        let repo = open_grit_repo(&racer_dir);
        let opts = RemoveOptions {
            pathspecs: vec!["base".to_owned()],
            cached: true,
            force: true,
            ..RemoveOptions::default()
        };
        remove_paths(&repo, &opts).map(|o| o.removed)
    });
    thread::sleep(HOLD_LOCK_FOR);
    write_entry_under_lock(dir, "a.txt", &mut lock);
    drop(lock);

    let removed = racer.join().expect("racer").expect("racing remove");
    assert_eq!(removed, vec!["base".to_owned()]);
    let staged = git_ls_files_stage0_map(dir);
    assert_eq!(
        staged.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a.txt"],
        "the entry written while the lock was held must survive"
    );
    assert_git_fsck_strict(dir);
}

#[test]
fn stage_tracked_reads_index_after_waiting_for_lock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    git(dir, &["config", "core.filesRefLockTimeout", "-1"]);
    fs::write(dir.join("a.txt"), "a\n").expect("write a");
    fs::write(dir.join("base"), "changed\n").expect("modify base");

    let repo = open_grit_repo(dir);
    let mut lock = repo.index_handle().begin_update().expect("hold index.lock");

    let racer_dir = dir.to_path_buf();
    let racer = thread::spawn(move || {
        let repo = open_grit_repo(&racer_dir);
        let work_tree = repo.work_tree().expect("work tree").to_path_buf();
        stage_tracked_modifications(&repo, &work_tree).map(|s| s.modified)
    });
    thread::sleep(HOLD_LOCK_FOR);
    write_entry_under_lock(dir, "a.txt", &mut lock);
    drop(lock);

    let modified = racer.join().expect("racer").expect("racing tracked stage");
    assert_eq!(modified, vec![b"base".to_vec()]);
    let staged = git_ls_files_stage0_map(dir);
    assert_eq!(
        staged.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a.txt", "base"],
        "the entry written while the lock was held must survive"
    );
    let base_blob = git(dir, &["hash-object", "base"]);
    assert_eq!(staged["base"].to_hex(), base_blob.trim());
    assert_git_fsck_strict(dir);
}

#[test]
fn stage_fails_without_writing_when_lock_stays_held() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    init_repo(dir);
    git(dir, &["config", "core.filesRefLockTimeout", "0"]);
    fs::write(dir.join("a.txt"), "a\n").expect("write a");

    let repo = open_grit_repo(dir);
    let lock = repo.index_handle().begin_update().expect("hold index.lock");
    let err = stage(&repo, &stage_paths(&["a.txt"]), &mut NullProgress)
        .expect_err("stage must not proceed while index.lock is held");
    assert!(is_lock_contention(&err), "unexpected error {err}");
    drop(lock);

    assert!(!dir.join(".git/index.lock").exists());
    let staged = git_ls_files_stage0_map(dir);
    assert_eq!(
        staged.keys().map(String::as_str).collect::<Vec<_>>(),
        ["base"]
    );
}
