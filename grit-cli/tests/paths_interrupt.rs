//! Signals during `grit mv` / `grit rm` must not leave index locks (issue #2433).

#![cfg(unix)]

mod interrupt_common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use grit_test_support::grit_cmd;
use interrupt_common::{git_dir_locks, sigterm, spawn_grit};

fn seed_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("mkdir");
    grit_cmd(&["init"]).in_dir(repo).suc();
    for i in 0..800 {
        let dir = repo.join(format!("d{:02}", i % 20));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join(format!("f{i:04}.txt")), format!("base {i}\n")).expect("write");
    }
    grit_cmd(&["commit", "-m", "seed"]).in_dir(repo).suc();
}

fn wait_for_lock_then_signal(repo: &Path, args: &[&str], deliver: fn(&Child)) -> (i32, String) {
    let index_before = fs::read(repo.join(".git/index")).expect("read index");
    let mut child = spawn_grit(repo, args);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        if repo.join(".git/index.lock").exists() {
            deliver(&child);
            break;
        }
        assert!(Instant::now() < deadline, "command never took index.lock");
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
fn sigterm_during_mv_leaves_no_lock() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    let (code, stderr) =
        wait_for_lock_then_signal(&repo, &["mv", "d00/f0000.txt", "d01/f0000.txt"], sigterm);
    assert_eq!(code, 130, "{stderr}");
    assert!(git_dir_locks(&repo).is_empty(), "mv left locks: {stderr}");
    assert!(stderr.contains("operation cancelled"), "{stderr}");
    grit_cmd(&["status", "--json"]).in_dir(&repo).suc();
}

#[test]
fn sigterm_during_rm_leaves_no_lock() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_repo(&repo);
    let mut args = vec!["rm".to_string(), "--force".to_string()];
    for i in 0..100 {
        args.push(format!("d{:02}/f{i:04}.txt", i % 20));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stderr) = wait_for_lock_then_signal(&repo, &arg_refs, sigterm);
    assert_eq!(code, 130, "{stderr}");
    assert!(git_dir_locks(&repo).is_empty(), "rm left locks: {stderr}");
    assert!(stderr.contains("operation cancelled"), "{stderr}");
    grit_cmd(&["status", "--json"]).in_dir(&repo).suc();
}
