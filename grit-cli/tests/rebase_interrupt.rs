//! Ctrl+C during `grit rebase` rolls back ref and index updates (issue #2489).

#![cfg(unix)]

mod interrupt_common;

use std::fs;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use grit_test_support::grit_cmd;
use interrupt_common::{git_dir_locks, sigint, spawn_grit};

const FILES: usize = 4000;
const ATTEMPTS: usize = 8;

fn head_oid(repo: &Path) -> String {
    let out = grit_cmd(&["resolve", "HEAD", "--json"]).in_dir(repo).suc();
    let v: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    v["results"][0]["oid"].as_str().expect("oid").to_owned()
}

fn rebase_fixture(base: &Path) -> (std::path::PathBuf, String) {
    let repo = base.join("repo");
    fs::create_dir_all(&repo).expect("mkdir");
    grit_cmd(&["init"]).in_dir(&repo).suc();
    fs::write(repo.join("base"), "base\n").expect("write");
    grit_cmd(&["commit", "-m", "base"]).in_dir(&repo).suc();
    grit_cmd(&["switch", "-c", "feat"]).in_dir(&repo).suc();
    fs::write(repo.join("feat.txt"), "f\n").expect("write");
    grit_cmd(&["commit", "-m", "feat"]).in_dir(&repo).suc();
    let feat_tip = head_oid(&repo);
    grit_cmd(&["switch", "main"]).in_dir(&repo).suc();
    fs::create_dir_all(repo.join("w")).expect("mkdir w");
    for i in 0..FILES {
        fs::write(repo.join(format!("w/f{i}.txt")), format!("{i}\n")).expect("write");
    }
    grit_cmd(&["commit", "-m", "wide"]).in_dir(&repo).suc();
    grit_cmd(&["switch", "feat"]).in_dir(&repo).suc();
    (repo, feat_tip)
}

fn spawn_rebase(repo: &Path) -> Child {
    spawn_grit(repo, &["rebase", "main"])
}

fn checkout_in_progress(repo: &Path) -> bool {
    repo.join(".git/index.lock").exists()
        || (0..10).any(|i| repo.join(format!("w/f{i}.txt")).exists())
}

#[test]
fn ctrl_c_during_rebase_restores_feat_tip_and_clears_locks() {
    let base = tempfile::tempdir().expect("tempdir");
    let (repo, feat_before) = rebase_fixture(base.path());

    let mut cancelled = false;
    for _ in 0..ATTEMPTS {
        let mut child = spawn_rebase(&repo);
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if child.try_wait().expect("poll").is_some() {
                break;
            }
            if checkout_in_progress(&repo) {
                sigint(&child);
                break;
            }
            assert!(Instant::now() < deadline, "rebase never started checkout");
            std::thread::sleep(Duration::from_micros(500));
        }
        let out = child.wait_with_output().expect("wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.code() == Some(130) && stderr.contains("operation cancelled") {
            cancelled = true;
            assert!(
                git_dir_locks(&repo).is_empty(),
                "{:?}",
                git_dir_locks(&repo)
            );
            assert_eq!(
                head_oid(&repo),
                feat_before,
                "HEAD moved during cancelled rebase"
            );
            break;
        }
    }
    assert!(cancelled, "never interrupted rebase mid-checkout");
}
