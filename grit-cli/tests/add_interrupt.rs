//! Ctrl+C during `grit add` removes index locks (issue #2433).

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

fn interrupt_add(repo: &Path, args: &[&str], deliver: fn(&Child)) -> (i32, String) {
    let index_before = fs::read(repo.join(".git/index")).expect("read index");
    let mut child = spawn_grit(repo, args);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        if index_locked(repo) {
            deliver(&child);
            break;
        }
        assert!(Instant::now() < deadline, "add never took index.lock");
        std::thread::sleep(Duration::from_micros(200));
    }
    let out = child.wait_with_output().expect("wait");
    assert_eq!(
        fs::read(repo.join(".git/index")).expect("read index after"),
        index_before,
        "index must be unchanged after interrupt"
    );
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn ctrl_c_during_add_leaves_no_lock_and_allows_retry() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    let (code, stderr) = interrupt_add(&repo, &["add"], sigint);
    assert_eq!(code, 130, "stderr: {stderr}");
    assert!(
        git_dir_locks(&repo).is_empty(),
        "interrupted add left locks {:?}: {stderr}",
        git_dir_locks(&repo)
    );
    assert!(
        stderr.contains("operation cancelled"),
        "expected cancellation message, got: {stderr}"
    );
    grit_cmd(&["add", "d00"]).in_dir(&repo).suc();
}

#[cfg(unix)]
#[test]
fn sigterm_and_sighup_during_add_leave_no_lock() {
    use interrupt_common::{sighup, sigterm};

    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    for deliver in [sigterm as fn(&Child), sighup] {
        let (code, stderr) = interrupt_add(&repo, &["add"], deliver);
        assert_eq!(code, 130, "{stderr}");
        assert!(
            git_dir_locks(&repo).is_empty(),
            "interrupted add left locks {:?}: {stderr}",
            git_dir_locks(&repo)
        );
        assert!(stderr.contains("operation cancelled"), "{stderr}");
    }
}

#[cfg(unix)]
#[test]
fn sigterm_during_add_patch_file_exits_130() {
    use interrupt_common::sigterm;

    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    fs::write(
        repo.join("stage.patch"),
        "diff --git a/d00/f0000.txt b/d00/f0000.txt\n\
         --- a/d00/f0000.txt\n\
         +++ b/d00/f0000.txt\n\
         @@ -1 +1 @@\n\
         -base 0\n\
         +staged\n",
    )
    .expect("write patch");
    let (code, stderr) = interrupt_add(&repo, &["add", "--patch-file", "stage.patch"], sigterm);
    assert_eq!(code, 130, "{stderr}");
    assert!(git_dir_locks(&repo).is_empty(), "{stderr}");
    assert!(stderr.contains("operation cancelled"), "{stderr}");
}
