//! Parallel clone into the same new destination (issue #1935).
//!
//! Exactly one clone must succeed; losers fail without removing the winner's tree.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use grit_lib::progress::NeverCancel;
use grit_lib::remote::clone::{clone as clone_repo, CloneError, CloneOptions};
use grit_lib::remote::fetch::NoProgress;

fn grit_bin() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_grit")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/grit")
        })
}

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn prepare_remote() -> (tempfile::TempDir, String) {
    let root = tempfile::tempdir().expect("tempdir");
    let bare = root.path().join("remote.git");
    git_in(root.path(), &["init", "-q", "--bare", "remote.git"]);
    let seed = root.path().join("seed");
    std::fs::create_dir(&seed).expect("mkdir seed");
    git_in(&seed, &["init", "-q", "-b", "main"]);
    git_in(&seed, &["config", "user.name", "a"]);
    git_in(&seed, &["config", "user.email", "a@b"]);
    std::fs::write(seed.join("f"), b"1\n").expect("write");
    git_in(&seed, &["add", "."]);
    git_in(&seed, &["commit", "-qm", "c1"]);
    git_in(&seed, &["push", "-q", "../remote.git", "main"]);
    let url = bare.to_string_lossy().into_owned();
    (root, url)
}

#[test]
fn parallel_clone_library_exactly_one_winner() {
    let (_root, url) = prepare_remote();
    let dest_root = tempfile::tempdir().expect("dest root");
    let dest = dest_root.path().join("pcl");

    let mut handles = Vec::new();
    for _ in 0..4 {
        let url = url.clone();
        let dest = dest.clone();
        handles.push(thread::spawn(move || {
            clone_repo(
                CloneOptions::new(url, dest),
                &mut NoProgress,
                &NeverCancel,
                None,
            )
        }));
    }

    let mut successes = 0usize;
    let mut dest_not_empty = 0usize;
    for handle in handles {
        match handle.join().expect("join") {
            Ok(_) => successes += 1,
            Err(CloneError::DestNotEmpty(_)) => dest_not_empty += 1,
            Err(other) => panic!("unexpected clone error: {other:?}"),
        }
    }
    assert_eq!(successes, 1, "expected one successful parallel clone");
    assert_eq!(dest_not_empty, 3, "expected three destination-busy losers");
    assert!(
        dest.join(".git").is_dir(),
        "winning clone must leave destination intact"
    );
    git_in(&dest, &["fsck"]);
}

#[test]
fn parallel_clone_cli_exactly_one_winner() {
    let grit = grit_bin();
    if !grit.is_file() {
        eprintln!(
            "skip parallel_clone_cli: grit binary missing at {}",
            grit.display()
        );
        return;
    }

    for round in 0..3 {
        let (_root, url) = prepare_remote();
        let dest_root = tempfile::tempdir().expect("dest root");
        let dest = dest_root.path().join("pcl");
        let dest_str = dest.to_string_lossy().into_owned();

        let (tx, rx) = mpsc::channel();
        for i in 0..4 {
            let grit = grit.clone();
            let url = url.clone();
            let dest_str = dest_str.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let out = Command::new(&grit)
                    .args(["clone", &url, &dest_str])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .expect("spawn grit clone");
                let _ = tx.send((i, out));
            });
        }
        drop(tx);

        let mut outputs = Vec::new();
        for _ in 0..4 {
            outputs.push(rx.recv().expect("child output"));
        }

        let successes = outputs.iter().filter(|(_, o)| o.status.success()).count();
        assert_eq!(
            successes, 1,
            "round {round}: expected one grit clone success, got {successes:?}"
        );
        assert!(
            dest.join(".git").is_dir(),
            "round {round}: destination must survive parallel clone"
        );
        for (_, out) in &outputs {
            if out.status.success() {
                continue;
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("already exists") || stderr.contains("not empty"),
                "round {round}: loser stderr should mention busy destination: {stderr}"
            );
        }
    }
}
