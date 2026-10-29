//! Ctrl+C during `grit commit` removes index locks (issue #2315).

#![cfg(any(unix, windows))]

mod interrupt_common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use grit_test_support::grit_cmd;
use interrupt_common::{git_dir_locks, sigint, spawn_grit};

const FILES: usize = 4000;
const DIRS: usize = 40;

fn seed_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("mkdir");
    grit_cmd(&["init"]).in_dir(repo).suc();
    for i in 0..FILES {
        let dir = repo.join(format!("d{:02}", i % DIRS));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join(format!("f{i:04}.txt")),
            format!("base {i}\n").repeat(50),
        )
        .expect("write");
    }
    grit_cmd(&["commit", "-m", "seed"]).in_dir(repo).suc();
    for i in 0..200 {
        let path = repo.join(format!("d{:02}/f{i:04}.txt", i % DIRS));
        fs::write(path, format!("dirty {i}\n").repeat(50)).expect("dirty");
    }
}

fn index_locked(repo: &Path) -> bool {
    repo.join(".git/index.lock").exists()
}

fn interrupt_commit(repo: &Path, deliver: fn(&Child)) -> String {
    let mut child = spawn_grit(repo, &["commit", "-m", "interrupt"]);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        if index_locked(repo) {
            deliver(&child);
            break;
        }
        assert!(Instant::now() < deadline, "commit never took index.lock");
        std::thread::sleep(Duration::from_micros(200));
    }
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn ctrl_c_during_commit_leaves_no_lock_and_allows_retry() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    let stderr = interrupt_commit(&repo, sigint);
    assert!(
        git_dir_locks(&repo).is_empty(),
        "interrupted commit left locks {:?}: {stderr}",
        git_dir_locks(&repo)
    );
    assert!(
        stderr.contains("operation cancelled"),
        "expected cancellation message, got: {stderr}"
    );
    grit_cmd(&["status", "--json"]).in_dir(&repo).suc();
    grit_cmd(&["commit", "-m", "after"]).in_dir(&repo).suc();
}

#[cfg(unix)]
#[test]
fn sigterm_and_sighup_during_commit_leave_no_lock() {
    use interrupt_common::{sighup, sigterm};

    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    for deliver in [sigterm as fn(&Child), sighup] {
        let stderr = interrupt_commit(&repo, deliver);
        assert!(
            git_dir_locks(&repo).is_empty(),
            "locks after signal: {stderr}"
        );
        assert!(stderr.contains("operation cancelled"), "{stderr}");
        grit_cmd(&["status"]).in_dir(&repo).suc();
    }
}
