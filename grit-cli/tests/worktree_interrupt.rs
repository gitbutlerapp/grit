//! Ctrl+C during `grit worktree add` rolls back the half-built worktree (issue #2489).

#![cfg(unix)]

mod interrupt_common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use grit_test_support::grit_cmd;
use interrupt_common::{git_dir_locks, sigint, spawn_grit};

const FILES: usize = 2500;

fn wide_fixture(base: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let repo = base.join("repo");
    fs::create_dir_all(&repo).expect("mkdir");
    grit_cmd(&["init"]).in_dir(&repo).suc();
    fs::create_dir_all(repo.join("w")).expect("mkdir w");
    for i in 0..FILES {
        fs::write(repo.join(format!("w/f{i}.txt")), format!("{i}\n")).expect("write");
    }
    grit_cmd(&["commit", "-m", "wide"]).in_dir(&repo).suc();
    let wt = base.join("wt1");
    (repo, wt)
}

fn checkout_started(wt: &Path) -> bool {
    wt.exists()
        && wt
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
}

fn spawn_worktree_add(repo: &Path, wt: &Path) -> Child {
    spawn_grit(
        repo,
        &["worktree", "add", wt.to_str().expect("utf8"), "-b", "wtb"],
    )
}

#[test]
fn ctrl_c_during_worktree_add_leaves_list_unchanged_and_no_locks() {
    let base = tempfile::tempdir().expect("tempdir");
    let (repo, wt) = wide_fixture(base.path());
    let list_before = grit_cmd(&["worktree", "list", "--json"])
        .in_dir(&repo)
        .suc()
        .stdout;

    let mut cancelled = false;
    for _ in 0..5 {
        let mut child = spawn_worktree_add(&repo, &wt);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if child.try_wait().expect("poll").is_some() {
                break;
            }
            if checkout_started(&wt) {
                sigint(&child);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worktree add never started checkout"
            );
            std::thread::sleep(Duration::from_micros(500));
        }
        let out = child.wait_with_output().expect("wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() == Some(130) && stderr.contains("operation cancelled") {
            cancelled = true;
            assert!(
                git_dir_locks(&repo).is_empty(),
                "locks left: {:?}",
                git_dir_locks(&repo)
            );
            let list_after = grit_cmd(&["worktree", "list", "--json"])
                .in_dir(&repo)
                .suc()
                .stdout;
            assert_eq!(list_before, list_after, "worktree list changed");
            assert!(
                !repo.join(".git/refs/heads/wtb").exists(),
                "branch wtb should not exist"
            );
            break;
        }
        let _ = fs::remove_dir_all(&wt);
    }
    assert!(cancelled, "never interrupted worktree add mid-checkout");
}
