//! Ctrl+C during `grit pick` removes index locks and leaves HEAD unchanged (issue #2315).

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
const CHANGED_DIRS: usize = 20;

fn head_oid(repo: &Path) -> String {
    let out = grit_cmd(&["resolve", "HEAD", "--json"]).in_dir(repo).suc();
    let resolved: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    resolved["results"][0]["oid"]
        .as_str()
        .expect("oid")
        .to_owned()
}

fn feature_tip(repo: &Path) -> String {
    let out = grit_cmd(&["resolve", "feature", "--json"])
        .in_dir(repo)
        .suc();
    let resolved: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    resolved["results"][0]["oid"]
        .as_str()
        .expect("oid")
        .to_owned()
}

fn seed_repo(repo: &Path) -> (String, String) {
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
    let main_head = head_oid(repo);
    grit_cmd(&["branch", "feature"]).in_dir(repo).suc();
    grit_cmd(&["switch", "feature"]).in_dir(repo).suc();
    for d in 0..CHANGED_DIRS {
        let dir = repo.join(format!("d{d:02}"));
        for entry in fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                fs::write(&path, "feature\n".repeat(50)).expect("write");
            }
        }
    }
    grit_cmd(&["commit", "-am", "feature"]).in_dir(repo).suc();
    let pick = feature_tip(repo);
    grit_cmd(&["switch", "main"]).in_dir(repo).suc();
    (main_head, pick)
}

fn pick_checking_out(repo: &Path) -> bool {
    repo.join("d00/f0000.txt").exists()
        && fs::read_to_string(repo.join("d00/f0000.txt"))
            .map(|s| s.starts_with("feature"))
            .unwrap_or(false)
}

fn interrupt_pick(repo: &Path, pick: &str, deliver: fn(&Child)) -> String {
    let mut child = spawn_grit(repo, &["pick", pick]);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("wait").is_some() {
            break;
        }
        if pick_checking_out(repo) {
            deliver(&child);
            break;
        }
        assert!(Instant::now() < deadline, "pick never started checkout");
        std::thread::sleep(Duration::from_micros(500));
    }
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn ctrl_c_during_pick_leaves_head_unchanged_and_no_lock() {
    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    let (head_before, pick) = seed_repo(&repo);
    let stderr = interrupt_pick(&repo, &pick, sigint);
    assert!(
        git_dir_locks(&repo).is_empty(),
        "interrupted pick left locks {:?}: {stderr}",
        git_dir_locks(&repo)
    );
    assert!(
        stderr.contains("operation cancelled"),
        "expected cancellation message, got: {stderr}"
    );
    assert_eq!(
        head_oid(&repo),
        head_before,
        "HEAD moved during interrupted pick"
    );
    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "CHERRY_PICK_HEAD left behind"
    );
    grit_cmd(&["status", "--json"]).in_dir(&repo).suc();
}

#[cfg(unix)]
#[test]
fn sigterm_and_sighup_during_pick_leave_no_lock() {
    use interrupt_common::{sighup, sigterm};

    let base = tempfile::tempdir().expect("tempdir");
    let repo = base.path().join("repo");
    let (head_before, pick) = seed_repo(&repo);
    for deliver in [sigterm as fn(&Child), sighup] {
        let stderr = interrupt_pick(&repo, &pick, deliver);
        assert!(git_dir_locks(&repo).is_empty(), "{stderr}");
        assert!(stderr.contains("operation cancelled"), "{stderr}");
        assert_eq!(head_oid(&repo), head_before);
        grit_cmd(&["status"]).in_dir(&repo).suc();
    }
}
