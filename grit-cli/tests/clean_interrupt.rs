//! Ctrl+C during `grit clean -f` exits 130 (issue #2489).

#![cfg(unix)]

mod interrupt_common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use grit_test_support::grit_cmd;
use interrupt_common::{sigint, spawn_grit};

const FILES: usize = 3000;

fn seed_untracked(repo: &Path) {
    fs::create_dir_all(repo).expect("mkdir");
    grit_cmd(&["init"]).in_dir(repo).suc();
    fs::write(repo.join("tracked"), "t\n").expect("write");
    grit_cmd(&["commit", "-m", "seed"]).in_dir(repo).suc();
    for i in 0..FILES {
        fs::write(repo.join(format!("u{i}.txt")), format!("{i}\n")).expect("write");
    }
}

fn removal_started(repo: &Path) -> bool {
    (0..50).any(|i| !repo.join(format!("u{i}.txt")).exists())
}

#[test]
fn ctrl_c_during_clean_force_exits_130() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    seed_untracked(&repo);

    let mut child = spawn_grit(&repo, &["clean", "-f"]);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("poll").is_some() {
            break;
        }
        if removal_started(&repo) {
            sigint(&child);
            break;
        }
        assert!(Instant::now() < deadline, "clean never started removing");
        std::thread::sleep(Duration::from_micros(500));
    }
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "stderr: {stderr}");
    assert!(stderr.contains("operation cancelled"), "stderr: {stderr}");
}
